//! Template Explorer — SSMS-style script templates with parameter
//! substitution, backed entirely by `sqlly-scripting`'s catalog (the GPUI
//! layer holds no template text).
//!
//! Browse by category or filter, fill in the template's parameters, watch the
//! rendered SQL preview update, and insert the result into a new editor tab.
//! Insertion goes through the shared tool-dialog contract; nothing executes.
//!
//! The left-hand template list is a `gpui_component::list::List`: category
//! groups map to list sections (empty sections vanish, so filtering still
//! collapses groups), rows keep the bespoke name + "user"-badge rendering via
//! a custom `ListDelegate` item, and clicking a row still routes through
//! `select_template`. The dialog keeps its own filter input (the kit's
//! built-in search stays off) and by-name selection as the source of truth.
//!
//! Alongside the read-only built-in catalog, the dialog owns a user template
//! catalog persisted as a versioned JSON file in the app's data area
//! (`user-templates.json` under `dirs::data_dir()/SQLLY` on native,
//! `localStorage` in the browser via `crate::host_storage`). "New Template…"
//! opens an inline editor; saving derives the parameter list from
//! `<name,type,default>` placeholders in the body. User templates can be
//! edited and deleted; built-ins cannot.

use std::collections::HashMap;

use gpui::{
    App, Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled,
    Subscription, WeakEntity, Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::list::{List, ListDelegate, ListState};
use gpui_component::{Disableable as _, IndexPath, Selectable};
use sqlly_services::{
    ScriptTemplate, TemplateExplorer, TemplateOrigin, UserTemplatesFile, parameters_from_body,
};

use crate::dialog::{Dialog, DialogFrame};
use crate::host_storage::{self, StorageArea};
use crate::theme::Theme;
use crate::tool_dialog::ToolDialogEvent;

impl EventEmitter<ToolDialogEvent> for TemplateExplorerDialog {}

/// File name of the user template catalog inside the Data storage area.
pub const USER_TEMPLATES_FILE: &str = "user-templates.json";

/// The user's template catalog for an action that is about to use it: a
/// missing file is an empty catalog, but a corrupt one is an error, so a
/// custom action never reports "no longer exists" for a template that is
/// only unreadable.
///
/// Served from an in-memory cache: the explorer context menu asks for the
/// catalog on every open, and reading the file there would block the UI
/// thread. Saves through the Template Explorer replace the cached copy, and
/// on native a directory watcher re-reads it (off the UI thread) when the
/// file changes on disk.
pub(crate) fn try_load_user_templates() -> UserCatalog {
    USER_TEMPLATE_CACHE.get_or_load(&user_catalog_key(), read_user_templates)
}

/// A cached read of the user template catalog; `Err` carries the message for
/// an unreadable (corrupt) file.
pub(crate) type UserCatalog = Result<std::sync::Arc<Vec<ScriptTemplate>>, String>;

/// Reads and parses the catalog from host storage. Blocking I/O: call it
/// through the cache.
fn read_user_templates() -> UserCatalog {
    match host_storage::load_text(StorageArea::Data, USER_TEMPLATES_FILE) {
        Some(text) if !text.trim().is_empty() => UserTemplatesFile::try_from_json(&text)
            .map(|file| std::sync::Arc::new(file.templates))
            .map_err(|e| {
                crate::localization::format(
                    "editor.template_explorer.catalog_read_failed",
                    &[("error", &e.to_string())],
                )
            }),
        _ => Ok(std::sync::Arc::new(Vec::new())),
    }
}

/// Where the catalog lives, so a cached copy is never served for a different
/// storage root (tests redirect the root per test).
fn user_catalog_key() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        host_storage::artifact_path(StorageArea::Data, USER_TEMPLATES_FILE)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
    #[cfg(target_arch = "wasm32")]
    {
        USER_TEMPLATES_FILE.to_string()
    }
}

static USER_TEMPLATE_CACHE: CatalogCache = CatalogCache::new();

/// In-memory copy of the user template catalog, keyed by its storage
/// location. Loads run outside the lock; a generation counter keeps a slow
/// load from overwriting a newer save or refresh that landed meanwhile.
struct CatalogCache {
    state: std::sync::Mutex<CatalogCacheState>,
}

struct CatalogCacheState {
    key: Option<String>,
    entry: Option<UserCatalog>,
    generation: u64,
}

impl CatalogCache {
    const fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(CatalogCacheState {
                key: None,
                entry: None,
                generation: 0,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CatalogCacheState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// The cached catalog for `key`, loading it with `load` on a miss.
    fn get_or_load(&self, key: &str, load: impl FnOnce() -> UserCatalog) -> UserCatalog {
        let generation = {
            let state = self.lock();
            if state.key.as_deref() == Some(key)
                && let Some(entry) = &state.entry
            {
                return entry.clone();
            }
            state.generation
        };
        self.load_into(key, generation, load)
    }

    /// Re-reads the catalog for `key` and replaces the cached copy, unless a
    /// save replaced it while the read ran. The previous copy stays served
    /// until the new one is in, so readers never fall back to disk.
    fn refresh(&self, key: &str, load: impl FnOnce() -> UserCatalog) -> UserCatalog {
        let generation = self.lock().generation;
        self.load_into(key, generation, load)
    }

    fn load_into(
        &self,
        key: &str,
        generation: u64,
        load: impl FnOnce() -> UserCatalog,
    ) -> UserCatalog {
        let loaded = load();
        let mut state = self.lock();
        if state.generation == generation {
            state.generation += 1;
            state.key = Some(key.to_string());
            state.entry = Some(loaded.clone());
            return loaded;
        }
        // Something newer landed while this load ran; serve that instead.
        match (&state.key, &state.entry) {
            (Some(k), Some(entry)) if k == key => entry.clone(),
            _ => loaded,
        }
    }

    /// Replaces the cached copy with a catalog that was just written.
    fn store(&self, key: &str, templates: Vec<ScriptTemplate>) {
        let mut state = self.lock();
        state.generation += 1;
        state.key = Some(key.to_string());
        state.entry = Some(Ok(std::sync::Arc::new(templates)));
    }
}

/// Starts the native watcher that keeps the cached catalog in step with
/// edits made outside the Template Explorer, and warms the cache off the UI
/// thread. Call once at startup; later calls are no-ops.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn start_user_template_cache() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("user-templates-watch".into())
            .spawn(watch_user_templates);
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start the user template watcher");
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn watch_user_templates() {
    use notify::RecursiveMode;
    use notify_debouncer_mini::{DebounceEventResult, new_debouncer};

    let key = user_catalog_key();
    if let Err(error) = USER_TEMPLATE_CACHE.get_or_load(&key, read_user_templates) {
        tracing::warn!(%error, "user template catalog");
    }
    let Some(path) = host_storage::artifact_path(StorageArea::Data, USER_TEMPLATES_FILE) else {
        return;
    };
    let Some(dir) = path.parent().map(std::path::Path::to_path_buf) else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(%error, "user template watcher: no data directory");
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = match new_debouncer(
        std::time::Duration::from_millis(300),
        move |result: DebounceEventResult| {
            let _ = tx.send(result);
        },
    ) {
        Ok(debouncer) => debouncer,
        Err(error) => {
            tracing::warn!(%error, "could not start the user template watcher");
            return;
        }
    };
    // The parent directory, not the file: saves replace the file by rename,
    // and a file watch would stop at the first replacement.
    if let Err(error) = debouncer.watcher().watch(&dir, RecursiveMode::NonRecursive) {
        tracing::warn!(%error, "could not watch the user template catalog");
        return;
    }
    for result in rx {
        match result {
            Ok(events) if events.iter().any(|e| is_user_catalog_event(&e.path, &path)) => {
                if let Err(error) = USER_TEMPLATE_CACHE.refresh(&key, read_user_templates) {
                    tracing::warn!(%error, "user template catalog changed on disk");
                }
            }
            Ok(_) => {}
            Err(error) => tracing::debug!(%error, "user template watcher error"),
        }
    }
}

/// Whether a watcher event in the catalog's directory is about the catalog
/// file itself (not the atomic-save temp file or another store).
#[cfg(not(target_arch = "wasm32"))]
fn is_user_catalog_event(event: &std::path::Path, catalog: &std::path::Path) -> bool {
    event.file_name() == catalog.file_name()
}

/// Persists the user's template catalog. Refuses to overwrite a corrupt on-disk
/// file with a freshly built default catalog.
pub(crate) fn save_user_templates(templates: &[ScriptTemplate]) -> Result<(), String> {
    if let Some(text) = host_storage::load_text(StorageArea::Data, USER_TEMPLATES_FILE)
        && !text.trim().is_empty()
    {
        UserTemplatesFile::try_from_json(&text)?;
    }
    let json = UserTemplatesFile::new(templates.to_vec()).to_json();
    // Native: temp file + fsync + rename, so a crash mid-save never leaves a
    // truncated catalog. The browser's localStorage write is already atomic.
    #[cfg(not(target_arch = "wasm32"))]
    let written = {
        let path = host_storage::artifact_path(StorageArea::Data, USER_TEMPLATES_FILE)
            .ok_or_else(|| crate::localization::text("editor.template_explorer.no_storage"))?;
        crate::fs_safe::write_atomic(&path, json.as_bytes()).map_err(|e| {
            crate::localization::format(
                "editor.template_explorer.save_failed",
                &[("error", &e.to_string())],
            )
        })
    };
    #[cfg(target_arch = "wasm32")]
    let written = host_storage::store_text(StorageArea::Data, USER_TEMPLATES_FILE, &json)
        .map_err(|e| e.to_string());
    if written.is_ok() {
        USER_TEMPLATE_CACHE.store(&user_catalog_key(), templates.to_vec());
    }
    written
}

/// The explorer-action spec the editor's controls describe: `None` when the
/// template is not an explorer action, an error when it is but no object
/// kind is ticked (an empty list would mean "every kind" to older builds, the
/// opposite of what unticking everything says).
fn explorer_action_from_editor(
    enabled: bool,
    kinds: [bool; 4],
    run: bool,
) -> Result<Option<sqlly_services::ExplorerActionSpec>, sqlly_core::DisplayMessage> {
    if !enabled {
        return Ok(None);
    }
    let kinds: Vec<_> = sqlly_services::ExplorerObjectKind::ALL
        .into_iter()
        .zip(kinds)
        .filter_map(|(kind, on)| on.then_some(kind))
        .collect();
    if kinds.is_empty() {
        return Err(sqlly_core::DisplayMessage::new(
            "editor.template_explorer.object_kind_required",
            "Tick at least one object kind for the explorer action",
        ));
    }
    Ok(Some(sqlly_services::ExplorerActionSpec {
        kinds,
        mode: if run {
            sqlly_services::ExplorerActionMode::Run
        } else {
            sqlly_services::ExplorerActionMode::Script
        },
    }))
}

/// Builds a user template from the editor's fields. Returns `None` when the
/// trimmed name is empty (nothing sensible to save). The parameter list is
/// derived from `<name,type,default>` placeholders in the body.
fn user_template_from_fields(
    name: &str,
    category: &str,
    description: &str,
    body: &str,
) -> Option<ScriptTemplate> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some(ScriptTemplate {
        name: name.to_string(),
        category: category.trim().to_string(),
        description: description.trim().to_string(),
        body: body.to_string(),
        parameters: parameters_from_body(body),
        origin: TemplateOrigin::User,
        explorer_action: None,
    })
}

/// Inserts or replaces (by name) a template in the user catalog, so saving
/// under an existing name edits that template instead of duplicating it.
fn upsert_user_template(list: &mut Vec<ScriptTemplate>, template: ScriptTemplate) {
    if let Some(existing) = list.iter_mut().find(|t| t.name == template.name) {
        *existing = template;
    } else {
        list.push(template);
    }
}

/// One parameter row: the template parameter's name and its live input.
struct ParamRow {
    name: String,
    description: String,
    input: Entity<InputState>,
    _sub: Subscription,
}

/// The inline "New Template" / "Edit Template" editor pane's inputs.
struct EditorPane {
    name_input: Entity<InputState>,
    category_input: Entity<InputState>,
    description_input: Entity<InputState>,
    /// Multiline body input (`InputState::multi_line` + `auto_grow`).
    body_input: Entity<TextareaState>,
    /// Explorer-action flag: the template joins matching objects' right-click
    /// menus (Custom Actions submenu).
    action_enabled: bool,
    /// Object kinds the action targets, indexed like
    /// [`sqlly_services::ExplorerObjectKind::ALL`]. All on by default.
    action_kinds: [bool; 4],
    /// Run immediately instead of scripting to a reviewed tab (the default).
    action_run: bool,
    /// Keeps the Save button's enabled state live as the name is typed.
    _name_sub: Subscription,
}

/// One same-height row of the template list: the template's name plus a dim
/// "user" origin badge. The trailing 1px padding stands in for the old
/// hand-rolled container's inter-row `gap`.
#[derive(IntoElement)]
struct TemplateRow {
    name: String,
    is_user: bool,
    selected: bool,
}

impl Selectable for TemplateRow {
    fn selected(self, _selected: bool) -> Self {
        // The highlight mirrors the dialog's by-name selection (which
        // save/delete can move without going through the list), not the
        // list's internal index — so the kit's flag is ignored.
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl RenderOnce for TemplateRow {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let s = Theme::scaled;
        let mut row = div()
            .px(px(s(6.0)))
            .py(px(s(3.0)))
            .rounded(px(s(4.0)))
            .cursor_pointer()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .bg(if self.selected {
                Theme::selection()
            } else {
                gpui::transparent_black()
            })
            .text_color(if self.selected {
                Theme::text()
            } else {
                Theme::text_dim()
            })
            .text_size(px(Theme::font_size_ratio(0.87)))
            .child(div().flex_1().min_w(px(0.0)).child(self.name));
        if self.is_user {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::font_size_ratio(0.67)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "editor.template_explorer.user_badge",
                    )),
            );
        }
        div().pb(px(s(1.0))).child(row)
    }
}

/// One rendered category group: its header plus the rows beneath it, in
/// catalog order.
#[derive(PartialEq)]
struct TemplateSection {
    category: String,
    /// (template name, is a user template).
    rows: Vec<(String, bool)>,
}

/// Kit-list delegate over the filtered catalog. The dialog rebuilds
/// `sections` on every render by grouping `catalog.search(filter)` runs of
/// equal category — exactly the grouping the old hand-rolled list drew — so
/// filtering collapses groups for free (empty sections are skipped by the
/// kit). Clicking a row (or arrow-keying onto one while the list has focus)
/// routes back into [`TemplateExplorerDialog::select_template`].
struct TemplateListDelegate {
    dialog: WeakEntity<TemplateExplorerDialog>,
    sections: Vec<TemplateSection>,
    /// Currently selected template name (mirror of the dialog's `selected`).
    selected: Option<String>,
}

impl TemplateListDelegate {
    fn row(&self, ix: IndexPath) -> Option<&(String, bool)> {
        self.sections.get(ix.section)?.rows.get(ix.row)
    }
}

impl ListDelegate for TemplateListDelegate {
    type Item = TemplateRow;

    fn sections_count(&self, _cx: &App) -> usize {
        self.sections.len().max(1)
    }

    fn items_count(&self, section: usize, _cx: &App) -> usize {
        self.sections.get(section).map_or(0, |s| s.rows.len())
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut gpui::Context<ListState<Self>>,
    ) -> Option<TemplateRow> {
        let (name, is_user) = self.row(ix)?.clone();
        Some(TemplateRow {
            selected: self.selected.as_deref() == Some(name.as_str()),
            name,
            is_user,
        })
    }

    fn render_section_header(
        &mut self,
        section: usize,
        _window: &mut Window,
        _cx: &mut gpui::Context<ListState<Self>>,
    ) -> Option<impl IntoElement> {
        let s = Theme::scaled;
        let category = self.sections.get(section)?.category.clone();
        Some(
            div()
                .pt(px(s(6.0)))
                .pb(px(s(1.0)))
                .text_size(px(Theme::font_size_ratio(0.67)))
                .text_color(Theme::text_dim())
                .child(category.to_uppercase()),
        )
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut gpui::Context<ListState<Self>>,
    ) -> impl IntoElement {
        let s = Theme::scaled;
        div()
            .p(px(s(8.0)))
            .text_color(Theme::text_dim())
            .text_size(px(Theme::font_size_ratio(0.87)))
            .child(crate::localization::text(
                "editor.template_explorer.no_matches",
            ))
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        cx: &mut gpui::Context<ListState<Self>>,
    ) {
        // A kit-internal deselect (`None`, e.g. Escape's Cancel) never clears
        // the dialog's selection — the old list had no deselect gesture.
        let Some(name) = ix.and_then(|ix| self.row(ix)).map(|(name, _)| name.clone()) else {
            return;
        };
        self.selected = Some(name.clone());
        if let Some(dialog) = self.dialog.upgrade() {
            dialog.update(cx, |dialog, cx| dialog.select_template(&name, cx));
        }
    }
}

pub struct TemplateExplorerDialog {
    catalog: TemplateExplorer,
    /// The user's own templates, as persisted (source of truth for saves).
    user_templates: Vec<ScriptTemplate>,
    filter: String,
    filter_input: Entity<InputState>,
    /// Kit list over the filtered catalog; its delegate's sections are
    /// re-derived from `catalog`/`filter` every render.
    template_list: Entity<ListState<TemplateListDelegate>>,
    selected: Option<String>,
    /// Inputs for the selected template's parameters, rebuilt on selection.
    param_rows: Vec<ParamRow>,
    /// Current parameter values (name → value), seeded from defaults.
    values: HashMap<String, String>,
    /// Selection changed from a non-render path; render rebuilds the rows
    /// (InputState construction needs the `&mut Window`).
    params_dirty: bool,
    /// When `Some`, the detail pane is replaced by the template editor.
    editor: Option<EditorPane>,
    /// When set, save/delete could not persist because the on-disk catalog is corrupt.
    persist_error: Option<String>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl TemplateExplorerDialog {
    pub fn new(window: &mut Window, cx: &mut gpui::Context<Self>) -> Self {
        Self::with_seed(None, window, cx)
    }

    /// Like [`Self::new`], but when `seed_body` is `Some` the dialog opens
    /// straight into the template editor with the body pre-filled — the hook
    /// for an app-level "Save Selection as Template" action.
    pub fn with_seed(
        seed_body: Option<String>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "editor.template_explorer.filter_placeholder",
            ))
        });
        let subscriptions = vec![cx.subscribe_in(
            &filter_input,
            window,
            |this: &mut Self, state, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.filter = state.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )];
        // A corrupt catalog is shown as empty but named, so the user knows
        // why their templates are missing (saves refuse to overwrite it).
        let (user_templates, load_error) = match try_load_user_templates() {
            Ok(templates) => (templates.as_ref().clone(), None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let template_list = {
            let dialog = cx.weak_entity();
            cx.new(|cx| {
                ListState::new(
                    TemplateListDelegate {
                        dialog,
                        sections: Vec::new(),
                        selected: None,
                    },
                    window,
                    cx,
                )
            })
        };
        let mut this = Self {
            catalog: TemplateExplorer::with_built_in_and(user_templates.clone()),
            user_templates,
            filter: String::new(),
            filter_input,
            template_list,
            selected: None,
            param_rows: Vec::new(),
            values: HashMap::new(),
            params_dirty: false,
            editor: None,
            focus_handle: cx.focus_handle(),
            persist_error: load_error,
            frame: DialogFrame::new(Theme::scaled(760.0), Theme::scaled(520.0)),
            _subscriptions: subscriptions,
        };
        if let Some(body) = seed_body {
            this.open_editor(None, Some(body), window, cx);
        }
        this
    }

    /// Opens the inline editor, pre-filled from `template` (editing a user
    /// template) or from `seed_body` (new template seeded with SQL).
    fn open_editor(
        &mut self,
        template: Option<&ScriptTemplate>,
        seed_body: Option<String>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let field = |window: &mut Window,
                     cx: &mut gpui::Context<Self>,
                     placeholder: &str,
                     value: String| {
            let placeholder = placeholder.to_string();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let (name, category, description, body) = match template {
            Some(t) => (
                t.name.clone(),
                t.category.clone(),
                t.description.clone(),
                t.body.clone(),
            ),
            None => (
                String::new(),
                String::new(),
                String::new(),
                seed_body.unwrap_or_default(),
            ),
        };
        let name_input = field(
            window,
            cx,
            &crate::localization::text("editor.template_explorer.name_placeholder"),
            name,
        );
        let name_sub = cx.subscribe_in(
            &name_input,
            window,
            |_this: &mut Self, _state, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    cx.notify();
                }
            },
        );
        self.editor = Some(EditorPane {
            name_input,
            category_input: field(
                window,
                cx,
                &crate::localization::text("editor.template_explorer.category_placeholder"),
                category,
            ),
            description_input: field(
                window,
                cx,
                &crate::localization::text("editor.template_explorer.description_placeholder"),
                description,
            ),
            body_input: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .soft_wrap(true)
                    .auto_grow(6, 12)
                    .placeholder(crate::localization::input_placeholder(
                        window,
                        cx,
                        "editor.template_explorer.body_placeholder",
                    ))
                    .default_value(body)
            }),
            action_enabled: template.and_then(|t| t.explorer_action.as_ref()).is_some(),
            action_kinds: template
                .and_then(|t| t.explorer_action.as_ref())
                .map(|spec| {
                    sqlly_services::ExplorerObjectKind::ALL
                        .map(|kind| spec.kinds.is_empty() || spec.kinds.contains(&kind))
                })
                .unwrap_or([true; 4]),
            action_run: template
                .and_then(|t| t.explorer_action.as_ref())
                .is_some_and(|spec| spec.mode == sqlly_services::ExplorerActionMode::Run),
            _name_sub: name_sub,
        });
        cx.notify();
    }

    /// Saves the editor's template into the user catalog, persists it, and
    /// reselects it in the refreshed list. No-op when the name is empty.
    fn save_editor(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let Some(mut template) = user_template_from_fields(
            &editor.name_input.read(cx).value(),
            &editor.category_input.read(cx).value(),
            &editor.description_input.read(cx).value(),
            &editor.body_input.read(cx).value(),
        ) else {
            return;
        };
        match explorer_action_from_editor(
            editor.action_enabled,
            editor.action_kinds,
            editor.action_run,
        ) {
            Ok(spec) => template.explorer_action = spec,
            Err(message) => {
                self.persist_error = Some(crate::localization::message(message));
                cx.notify();
                return;
            }
        }
        let name = template.name.clone();
        upsert_user_template(&mut self.user_templates, template);
        match save_user_templates(&self.user_templates) {
            Ok(()) => {
                self.persist_error = None;
                self.rebuild_catalog();
                self.editor = None;
                self.selected = None;
                self.select_template(&name, cx);
            }
            Err(error) => {
                self.persist_error = Some(error);
                cx.notify();
            }
        }
    }

    /// Deletes the selected user template from the catalog and disk.
    fn delete_selected(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(name) = self.selected.clone() else {
            return;
        };
        if !matches!(
            self.selected_template().map(|t| t.origin),
            Some(TemplateOrigin::User)
        ) {
            return;
        }
        self.user_templates.retain(|t| t.name != name);
        match save_user_templates(&self.user_templates) {
            Ok(()) => {
                self.persist_error = None;
                self.rebuild_catalog();
                self.selected = None;
                self.values.clear();
                self.params_dirty = true;
            }
            Err(error) => self.persist_error = Some(error),
        }
        cx.notify();
    }

    fn rebuild_catalog(&mut self) {
        self.catalog = TemplateExplorer::with_built_in_and(self.user_templates.clone());
    }

    fn select_template(&mut self, name: &str, cx: &mut gpui::Context<Self>) {
        if self.selected.as_deref() == Some(name) {
            return;
        }
        self.selected = Some(name.to_string());
        self.values = self
            .catalog
            .find(name)
            .map(|t| {
                t.parameters
                    .iter()
                    .map(|p| (p.name.clone(), p.default_value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        self.params_dirty = true;
        cx.notify();
    }

    fn selected_template(&self) -> Option<&ScriptTemplate> {
        self.selected.as_deref().and_then(|n| self.catalog.find(n))
    }

    fn rendered_sql(&self) -> Option<String> {
        self.selected_template().map(|t| t.render(&self.values))
    }

    fn insert_into_editor(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(sql) = self.rendered_sql() {
            cx.emit(ToolDialogEvent::ScriptToEditor { sql });
            cx.emit(ToolDialogEvent::Close);
        }
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            // Escape backs out of the inline editor before closing the dialog.
            // Stop propagation either way so the chrome's own escape-to-close
            // (which would skip the editor stage) never doubles up.
            cx.stop_propagation();
            if self.editor.is_some() {
                self.editor = None;
                cx.notify();
            } else {
                self.close(cx);
            }
        }
    }

    /// Rebuild the parameter input rows for the current selection. Runs at
    /// render time because `InputState::new` needs the window.
    fn rebuild_param_rows(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        self.param_rows.clear();
        let Some(template) = self.selected_template().cloned() else {
            return;
        };
        for param in &template.parameters {
            let input =
                cx.new(|cx| InputState::new(window, cx).default_value(param.default_value.clone()));
            let name = param.name.clone();
            let sub = cx.subscribe_in(
                &input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.values
                            .insert(name.clone(), state.read(cx).value().to_string());
                        cx.notify();
                    }
                },
            );
            self.param_rows.push(ParamRow {
                name: param.name.clone(),
                description: param.description.clone(),
                input,
                _sub: sub,
            });
        }
    }
}

impl gpui::Focusable for TemplateExplorerDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TemplateExplorerDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        if self.params_dirty {
            self.params_dirty = false;
            self.rebuild_param_rows(window, cx);
        }

        let selected = self.selected.clone();
        let matches: Vec<ScriptTemplate> = self
            .catalog
            .search(&self.filter)
            .into_iter()
            .cloned()
            .collect();

        // Left: template list grouped by category (filter collapses groups),
        // rendered by the kit list. Consecutive-category runs of the filtered
        // matches become sections; the delegate is refreshed here so its
        // sections always track the catalog and filter.
        let mut sections: Vec<TemplateSection> = Vec::new();
        for template in &matches {
            if sections.last().map(|s| s.category.as_str()) != Some(template.category.as_str()) {
                sections.push(TemplateSection {
                    category: template.category.clone(),
                    rows: Vec::new(),
                });
            }
            sections
                .last_mut()
                .expect("a section was just pushed")
                .rows
                .push((
                    template.name.clone(),
                    template.origin == TemplateOrigin::User,
                ));
        }
        self.template_list.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            if delegate.sections != sections || delegate.selected != selected {
                delegate.sections = sections;
                delegate.selected = selected;
                cx.notify();
            }
        });
        let list = div()
            .id("template-list")
            .w(px(s(220.0)))
            .flex_shrink_0()
            .border_r_1()
            .border_color(Theme::border())
            .pr(px(s(6.0)))
            // The old hand-rolled list scrolled without a scrollbar overlay;
            // keep that look.
            .child(List::new(&self.template_list).scrollbar_visible(false));

        // Right: the inline template editor when open, otherwise the selected
        // template's description, parameters, and preview.
        let detail: gpui::AnyElement = if let Some(editor) = &self.editor {
            let labeled = |label: String, input: &Entity<InputState>| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .w(px(s(90.0)))
                            .flex_shrink_0()
                            .text_size(px(Theme::font_size_ratio(0.80)))
                            .text_color(Theme::text_dim())
                            .child(label),
                    )
                    .child(div().flex_1().child(Input::new(input)))
            };
            div()
                .id("template-editor")
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .gap(px(s(8.0)))
                .overflow_scroll()
                .child(labeled(
                    crate::localization::text("editor.template_explorer.name_label"),
                    &editor.name_input,
                ))
                .child(labeled(
                    crate::localization::text("editor.template_explorer.category_label"),
                    &editor.category_input,
                ))
                .child(labeled(
                    crate::localization::text("editor.template_explorer.description_label"),
                    &editor.description_input,
                ))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "editor.template_explorer.body_help",
                        )),
                )
                .child(Textarea::new(&editor.body_input))
                .child(
                    gpui_component::checkbox::Checkbox::new("te-action-enabled")
                        .label(crate::localization::text(
                            "editor.template_explorer.action_enabled_label",
                        ))
                        .checked(editor.action_enabled)
                        .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                            if let Some(editor) = &mut this.editor {
                                editor.action_enabled = *checked;
                            }
                            cx.notify();
                        })),
                )
                .when(editor.action_enabled, |d| {
                    d.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(10.0)))
                            .children(
                                sqlly_services::ExplorerObjectKind::ALL
                                    .into_iter()
                                    .enumerate()
                                    .map(|(index, kind)| {
                                        gpui_component::checkbox::Checkbox::new(
                                            gpui::SharedString::from(format!(
                                                "te-action-kind-{index}"
                                            )),
                                        )
                                        .label(kind.plural_label())
                                        .checked(editor.action_kinds[index])
                                        .on_click(
                                            cx.listener(
                                                move |this, checked: &bool, _window, cx| {
                                                    if let Some(editor) = &mut this.editor {
                                                        editor.action_kinds[index] = *checked;
                                                    }
                                                    cx.notify();
                                                },
                                            ),
                                        )
                                    }),
                            ),
                    )
                    .child(
                        gpui_component::checkbox::Checkbox::new("te-action-run")
                            .label(crate::localization::text(
                                "editor.template_explorer.action_run_label",
                            ))
                            .checked(editor.action_run)
                            .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                                if let Some(editor) = &mut this.editor {
                                    editor.action_run = *checked;
                                }
                                cx.notify();
                            })),
                    )
                })
                .into_any_element()
        } else if let Some(template) = self.selected_template().cloned() {
            let preview = self.rendered_sql().unwrap_or_default();
            let mut detail = div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(s(8.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(template.description.clone()),
                );
            for row in &self.param_rows {
                detail = detail.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(8.0)))
                        .child(
                            div()
                                .w(px(s(140.0)))
                                .flex_shrink_0()
                                .text_size(px(Theme::font_size_ratio(0.80)))
                                .text_color(Theme::text_dim())
                                .child(format!("{} — {}", row.name, row.description)),
                        )
                        .child(div().flex_1().child(Input::new(&row.input))),
                );
            }
            detail = detail.child(
                div()
                    .id("template-preview")
                    .flex_1()
                    .min_h(px(s(80.0)))
                    .overflow_scroll()
                    .p(px(s(8.0)))
                    .bg(Theme::sidebar_bg())
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::kind_string())
                    .whitespace_normal()
                    .child(preview),
            );
            detail.into_any_element()
        } else {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "editor.template_explorer.select_template_empty",
                ))
                .into_any_element()
        };

        // Footer buttons per phase: browse keeps Edit/Delete far left as
        // leading buttons and the chrome's default Close replaces the old
        // plain Close; edit swaps to Cancel/Save with no default Close.
        let mut leading_buttons: Vec<gpui::AnyElement> = Vec::new();
        let mut buttons: Vec<gpui::AnyElement> = Vec::new();
        if let Some(editor) = &self.editor {
            let can_save = !editor.name_input.read(cx).value().trim().is_empty()
                && explorer_action_from_editor(
                    editor.action_enabled,
                    editor.action_kinds,
                    editor.action_run,
                )
                .is_ok();
            buttons.push(
                Button::new("te-cancel-edit")
                    .label(crate::localization::text(
                        "editor.template_explorer.cancel_edit_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.editor = None;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            buttons.push(
                Button::new("te-save")
                    .primary()
                    .label(crate::localization::text(
                        "editor.template_explorer.save_button",
                    ))
                    .disabled(!can_save)
                    .on_click(cx.listener(|this, _, _window, cx| this.save_editor(cx)))
                    .into_any_element(),
            );
        } else {
            let can_insert = self.selected.is_some();
            let selected_user = self
                .selected_template()
                .filter(|t| t.origin == TemplateOrigin::User)
                .cloned();
            if let Some(user_template) = selected_user {
                leading_buttons.push(
                    Button::new("te-edit")
                        .label(crate::localization::text(
                            "editor.template_explorer.edit_button",
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_editor(Some(&user_template), None, window, cx)
                        }))
                        .into_any_element(),
                );
                leading_buttons.push(
                    Button::new("te-delete")
                        .label(crate::localization::text(
                            "editor.template_explorer.delete_button",
                        ))
                        .on_click(cx.listener(|this, _, _window, cx| this.delete_selected(cx)))
                        .into_any_element(),
                );
            }
            buttons.push(
                Button::new("te-insert")
                    .primary()
                    .label(crate::localization::text(
                        "editor.template_explorer.insert_button",
                    ))
                    .disabled(!can_insert)
                    .on_click(cx.listener(|this, _, _window, cx| this.insert_into_editor(cx)))
                    .into_any_element(),
            );
        }

        // The escape handler rides on the body so the two-stage behavior
        // (exit the inline editor first) runs before the chrome's own
        // escape-to-close; ✕/scrim/Close still fully close via on_close.
        let content = div()
            .id("template-explorer-content")
            .key_context("TemplateExplorerDialog")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .p(px(s(14.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(div().flex_1())
                    .child(
                        Button::new("te-new")
                            .label(crate::localization::text(
                                "editor.template_explorer.new_button",
                            ))
                            .disabled(self.editor.is_some())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_editor(None, None, window, cx)
                            })),
                    )
                    .child(div().w(px(s(220.0))).child(Input::new(&self.filter_input))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.0))
                    .gap(px(s(10.0)))
                    .child(list)
                    .child(detail),
            )
            .children(self.persist_error.clone().map(|error| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::error())
                    .child(error)
            }));

        let mut dialog = Dialog::new("template-explorer-dialog", |this: &mut Self| {
            &mut this.frame
        })
        .title(crate::localization::text(
            "editor.template_explorer.dialog_title",
        ))
        .easy_close(true)
        .bare_body()
        .use_default_close(self.editor.is_none())
        .dismiss_on_scrim_click(true)
        .on_close(|this, _window, cx| this.close(cx))
        .child(content);
        for button in leading_buttons {
            dialog = dialog.leading_button(button);
        }
        for button in buttons {
            dialog = dialog.button(button);
        }
        dialog.build(self, window, cx)
    }
}

// ── Custom-action parameter prompt ─────────────────────────────────────────

/// What the custom-action parameter prompt reports to the workbench.
#[derive(Clone, Debug, PartialEq)]
pub enum TemplateParamsEvent {
    /// The user confirmed: every parameter's value (raw text, keyed by
    /// parameter name), ready for `ScriptTemplate::render_for_object`.
    Submit(HashMap<String, String>),
    Closed,
}

/// The small "Specify values for template parameters" prompt a custom
/// explorer action shows before scripting or running a template that has
/// `<name,type,default>` placeholders. One input per parameter, prefilled with
/// its default; values substitute as raw text (SSMS template semantics), and a
/// Run action's rendered SQL still goes through the read-only check and the
/// review gate afterwards. Inputs are materialized at render time (renders
/// have the `Window` the explorer event handler lacks).
pub struct TemplateParamsDialog {
    template_name: String,
    /// Where the action will script or run, for the prompt's context line.
    target: String,
    run: bool,
    parameters: Vec<sqlly_services::scripting::TemplateParameter>,
    inputs: Vec<Entity<InputState>>,
    submitted: bool,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TemplateParamsEvent> for TemplateParamsDialog {}

impl TemplateParamsDialog {
    pub fn new(template: &ScriptTemplate, target: String, run: bool) -> Self {
        Self {
            template_name: template.name.clone(),
            target,
            run,
            parameters: template.parameters.clone(),
            inputs: Vec::new(),
            submitted: false,
            frame: DialogFrame::new_auto(Theme::scaled(480.0)),
            _subscriptions: Vec::new(),
        }
    }

    fn ensure_inputs(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if !self.inputs.is_empty() || self.parameters.is_empty() {
            return;
        }
        for param in &self.parameters {
            let input =
                cx.new(|cx| InputState::new(window, cx).default_value(param.default_value.clone()));
            let sub = cx.subscribe(&input, |this: &mut Self, _state, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit(cx);
                }
            });
            self.inputs.push(input);
            self._subscriptions.push(sub);
        }
        if let Some(first) = self.inputs.first() {
            first.update(cx, |state, cx| state.focus(window, cx));
        }
    }

    /// The values as typed, keyed by parameter name. Inputs not built yet
    /// (the prompt never rendered) contribute their defaults.
    pub fn values(&self, cx: &App) -> HashMap<String, String> {
        self.parameters
            .iter()
            .enumerate()
            .map(|(i, param)| {
                let value = self
                    .inputs
                    .get(i)
                    .map(|input| input.read(cx).value().to_string())
                    .unwrap_or_else(|| param.default_value.clone());
                (param.name.clone(), value)
            })
            .collect()
    }

    /// Confirm once: Enter in an input and the footer's confirm can both land
    /// for the same keypress, and a Run action must never fire twice.
    pub fn submit(&mut self, cx: &mut gpui::Context<Self>) {
        if self.submitted {
            return;
        }
        self.submitted = true;
        let values = self.values(cx);
        cx.emit(TemplateParamsEvent::Submit(values));
    }
}

impl Render for TemplateParamsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        self.ensure_inputs(window, cx);
        let verb = if self.run {
            crate::localization::text("editor.template_explorer.params_run_button")
        } else {
            crate::localization::text("editor.template_explorer.params_script_button")
        };
        let mut body = div().flex().flex_col().gap(px(s(8.0))).child(
            div()
                .text_size(px(Theme::font_size_ratio(0.82)))
                .text_color(Theme::text_dim())
                .child(crate::localization::format(
                    if self.run {
                        "editor.template_explorer.params_run_description"
                    } else {
                        "editor.template_explorer.params_script_description"
                    },
                    &[("target", &self.target)],
                )),
        );
        for (param, input) in self.parameters.iter().zip(&self.inputs) {
            let label = match (param.data_type.trim(), param.description.trim()) {
                ("", "") => param.name.clone(),
                (ty, "") => format!("{} ({ty})", param.name),
                ("", desc) => format!("{} — {desc}", param.name),
                (ty, desc) => format!("{} ({ty}) — {desc}", param.name),
            };
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .w(px(s(160.0)))
                            .flex_shrink_0()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text())
                            .child(label),
                    )
                    .child(div().flex_1().min_w(px(0.0)).child(Input::new(input))),
            );
        }
        Dialog::new("template-params-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::format(
                "editor.template_explorer.params_dialog_title",
                &[("template", &self.template_name)],
            ))
            .confirm(
                verb,
                crate::localization::text("editor.template_explorer.params_cancel_button"),
            )
            .on_confirm(|this, _window, cx| this.submit(cx))
            .on_reject(|_this, _window, cx| cx.emit(TemplateParamsEvent::Closed))
            .on_close(|_this, _window, cx| cx.emit(TemplateParamsEvent::Closed))
            .child(body)
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tpl(name: &str) -> ScriptTemplate {
        user_template_from_fields(name, "c", "", "SELECT 1").unwrap()
    }

    fn names(catalog: &UserCatalog) -> Vec<String> {
        catalog
            .as_ref()
            .expect("readable catalog")
            .iter()
            .map(|t| t.name.clone())
            .collect()
    }

    #[test]
    fn catalog_cache_loads_once_then_serves_memory() {
        let cache = CatalogCache::new();
        let loads = std::cell::Cell::new(0);
        let load = || {
            loads.set(loads.get() + 1);
            Ok(std::sync::Arc::new(vec![tpl("a")]))
        };
        assert_eq!(names(&cache.get_or_load("k", load)), ["a"]);
        assert_eq!(names(&cache.get_or_load("k", load)), ["a"]);
        assert_eq!(loads.get(), 1);
        // A different storage location is a miss.
        cache.get_or_load("other", load).unwrap();
        assert_eq!(loads.get(), 2);
    }

    #[test]
    fn catalog_cache_store_replaces_without_reading() {
        let cache = CatalogCache::new();
        cache
            .get_or_load("k", || Ok(std::sync::Arc::new(vec![tpl("old")])))
            .unwrap();
        cache.store("k", vec![tpl("new")]);
        let served = cache.get_or_load("k", || panic!("a stored catalog must not be re-read"));
        assert_eq!(names(&served), ["new"]);
    }

    #[test]
    fn catalog_cache_keeps_a_corrupt_catalog_as_an_error() {
        let cache = CatalogCache::new();
        let first = cache.get_or_load("k", || Err("corrupt".into()));
        assert_eq!(first.unwrap_err(), "corrupt");
        // Still an error on the next read, not an empty catalog.
        let again = cache.get_or_load("k", || panic!("cached"));
        assert_eq!(again.unwrap_err(), "corrupt");
        // A refresh after the file is fixed replaces it.
        cache
            .refresh("k", || Ok(std::sync::Arc::new(vec![tpl("fixed")])))
            .unwrap();
        assert_eq!(
            names(&cache.get_or_load("k", || panic!("cached"))),
            ["fixed"]
        );
    }

    #[test]
    fn catalog_cache_load_never_overwrites_a_save_that_landed_meanwhile() {
        let cache = CatalogCache::new();
        // A refresh that read the file just before a save completed.
        let served = cache.refresh("k", || {
            cache.store("k", vec![tpl("saved")]);
            Ok(std::sync::Arc::new(vec![tpl("stale")]))
        });
        assert_eq!(names(&served), ["saved"]);
        assert_eq!(
            names(&cache.get_or_load("k", || panic!("cached"))),
            ["saved"]
        );
    }

    #[test]
    fn catalog_watcher_only_reacts_to_the_catalog_file() {
        let dir = std::env::temp_dir();
        let catalog = dir.join(USER_TEMPLATES_FILE);
        assert!(is_user_catalog_event(&catalog, &catalog));
        assert!(!is_user_catalog_event(
            &dir.join(format!(".{USER_TEMPLATES_FILE}.1234.tmp")),
            &catalog
        ));
        assert!(!is_user_catalog_event(&dir.join("history.json"), &catalog));
    }

    #[test]
    fn saved_templates_are_served_from_memory_and_corrupt_files_surface() {
        let dir = tempfile::tempdir().unwrap();
        let _storage = host_storage::test_support::redirect_storage(dir.path().to_path_buf());
        let path = host_storage::artifact_path(StorageArea::Data, USER_TEMPLATES_FILE).unwrap();

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let err = try_load_user_templates().unwrap_err();
        assert!(err.contains("could not be read"), "{err}");
        // A corrupt file is never overwritten, and the cache keeps the error.
        assert!(save_user_templates(&[tpl("x")]).is_err());
        assert!(try_load_user_templates().is_err());

        std::fs::remove_file(&path).unwrap();
        USER_TEMPLATE_CACHE
            .refresh(&user_catalog_key(), read_user_templates)
            .unwrap();
        save_user_templates(&[tpl("mine")]).unwrap();
        // The saved catalog is served from memory: removing the file behind
        // the cache's back doesn't change what the menu sees.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(names(&try_load_user_templates()), ["mine"]);
    }

    #[test]
    fn built_in_catalog_covers_the_headline_categories() {
        let catalog = TemplateExplorer::with_built_in();
        let categories: Vec<String> = catalog.categories().into_iter().map(|c| c.name).collect();
        for expected in ["Table", "View", "Index", "Function", "Stored Procedure"] {
            assert!(
                categories.iter().any(|c| c == expected),
                "missing category {expected}"
            );
        }
    }

    #[test]
    fn rendering_with_defaults_substitutes_every_placeholder() {
        let catalog = TemplateExplorer::with_built_in();
        for template in catalog.templates() {
            let defaults: HashMap<String, String> = template
                .parameters
                .iter()
                .map(|p| (p.name.clone(), p.default_value.clone()))
                .collect();
            let rendered = template.render(&defaults);
            assert!(
                !rendered.contains('<') || !rendered.contains(",,"),
                "{} left placeholders behind: {rendered}",
                template.name
            );
        }
    }

    #[test]
    fn user_template_from_fields_derives_parameters_from_placeholders() {
        let template = user_template_from_fields(
            "  Top N  ",
            "Snippets",
            "First N rows",
            "SELECT TOP <top_n,int,10> * FROM <table_name,sysname,MyTable>",
        )
        .expect("named template saves");
        assert_eq!(template.name, "Top N");
        assert_eq!(template.origin, TemplateOrigin::User);
        assert_eq!(
            template
                .parameters
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["top_n", "table_name"]
        );
        assert_eq!(template.parameters[0].default_value, "10");
    }

    #[test]
    fn user_template_from_fields_rejects_empty_names() {
        assert!(user_template_from_fields("   ", "c", "d", "SELECT 1").is_none());
    }

    #[test]
    fn upsert_replaces_by_name_and_appends_new() {
        let mine =
            |name: &str, body: &str| user_template_from_fields(name, "", "", body).expect("named");
        let mut list = vec![mine("A", "SELECT 1")];
        upsert_user_template(&mut list, mine("B", "SELECT 2"));
        assert_eq!(list.len(), 2);
        upsert_user_template(&mut list, mine("A", "SELECT 99"));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].body, "SELECT 99");
    }

    #[test]
    fn user_catalog_round_trips_through_the_persisted_json_shape() {
        let saved =
            vec![user_template_from_fields("Mine", "", "", "SELECT <n,int,1>").expect("named")];
        let json = UserTemplatesFile::new(saved.clone()).to_json();
        let loaded = UserTemplatesFile::from_json(&json).templates;
        assert_eq!(loaded, saved);
        // The merged catalog files the category-less template under the
        // default user category, ahead of built-ins, with a "user" origin.
        let catalog = TemplateExplorer::with_built_in_and(loaded);
        assert_eq!(catalog.templates()[0].name, "Mine");
        assert_eq!(catalog.templates()[0].origin, TemplateOrigin::User);
        assert_eq!(catalog.categories()[0].name, "My Templates");
    }

    #[test]
    fn explorer_action_with_no_kinds_is_refused() {
        assert_eq!(
            explorer_action_from_editor(false, [false; 4], false),
            Ok(None)
        );
        assert!(explorer_action_from_editor(true, [false; 4], true).is_err());
        let spec = explorer_action_from_editor(true, [false, true, false, true], true)
            .unwrap()
            .unwrap();
        assert_eq!(
            spec.kinds,
            vec![
                sqlly_services::ExplorerObjectKind::View,
                sqlly_services::ExplorerObjectKind::Function
            ]
        );
        assert_eq!(spec.mode, sqlly_services::ExplorerActionMode::Run);
    }

    /// The custom-action prompt builds one input per parameter prefilled with
    /// its default, reports what was typed keyed by parameter name, and
    /// submits exactly once however many confirm paths fire.
    #[gpui::test]
    fn params_prompt_reports_typed_values_once(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let body = "SELECT TOP (<top,int,10>) * FROM {object} WHERE <filter,,1=1>";
        let template = user_template_from_fields("Peek", "", "", body).expect("template");
        let (dialog, cx) = cx.add_window_view(|_window, _cx| {
            TemplateParamsDialog::new(&template, "prod \u{b7} shop".into(), true)
        });
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = events.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&dialog, move |_, event: &TemplateParamsEvent, _| {
                sink.borrow_mut().push(event.clone());
            })
            .detach();
        });
        // Before the first render, the values are the defaults.
        dialog.update(cx, |d, cx| {
            let values = d.values(cx);
            assert_eq!(values.get("top").map(String::as_str), Some("10"));
            assert_eq!(values.get("filter").map(String::as_str), Some("1=1"));
        });
        cx.run_until_parked();
        dialog.update_in(cx, |d, window, cx| {
            d.ensure_inputs(window, cx);
            assert_eq!(d.inputs.len(), 2);
            assert_eq!(d.inputs[0].read(cx).value().as_ref(), "10");
            let filter = d.inputs[1].clone();
            filter.update(cx, |state, cx| state.set_value("Freight > 5", window, cx));
            d.submit(cx);
            d.submit(cx);
        });
        cx.run_until_parked();
        let events = events.borrow();
        assert_eq!(events.len(), 1, "a second confirm must not submit again");
        let TemplateParamsEvent::Submit(values) = &events[0] else {
            panic!("expected a submit, got {:?}", events[0]);
        };
        assert_eq!(values.get("top").map(String::as_str), Some("10"));
        assert_eq!(
            values.get("filter").map(String::as_str),
            Some("Freight > 5")
        );
    }
}
