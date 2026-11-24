//! Notebook parameters in the shell: the widgets parameter cells and the
//! parameters rail show, where each value is kept (the notebook file for
//! shared values, a personal side file for personal ones and secret picks,
//! memory only for sensitive ones), Apply gating, query-filled and linked
//! options, presets and named sets, and the definition editor.
//!
//! A change to an `auto` parameter is stored after a short debounce and
//! marks its dependents stale; in reactive mode cheap local cells re-run at
//! once and remote cells wait in the Apply bar. An `apply` parameter's
//! change is held as a draft until Apply.

pub mod editor;
mod render;
pub mod store;
pub mod widget;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use gpui::{AppContext as _, Context, Entity, Subscription, Task, Window};
use serde_json::Value;
use sqlly_services::notebook::cells::parameter::{
    self as spec, BoundOptions, OptionItem, OptionsPage, OptionsQuery, ParameterCellSpec,
    PersonalParams, SetTarget,
};
use sqlly_services::notebook::params::{
    self, AllowedValues, Clock, ParamError, ParamType, ParamValue, ParameterDef, Preset,
    SystemClock,
};
use sqlly_services::notebook::{CellKind, Node, RunMode};
use sqlly_services::notebook_security::TrustLevel;

use self::editor::{Field, ParamEditor, ParamEditorEvent, SlotChoice};
use self::store::{KeyringSecrets, SecretCatalog};
use self::widget::{
    Edit, MenuEntry, OptionsState, ParamWidget, ParamWidgetEvent, PresetPick, Surface, WidgetStatus,
};
use super::connection::CellRunTarget;
use super::doc_adapter::RunScope;
use super::view::{NotebookEvent, NotebookView};

/// How long typing pauses before a value is taken.
pub const PARAM_DEBOUNCE: Duration = Duration::from_millis(400);
/// Statement timeout of a discovery query.
pub const OPTIONS_TIMEOUT_SECONDS: u32 = 30;
/// Discovery results kept per notebook, by fingerprint.
const OPTIONS_CACHE: usize = 64;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn message(error: &ParamError) -> String {
    crate::localization::message(error.message())
}

/// A discovery query for the host to run read-only on `target`.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamOptionsRequest {
    pub param: String,
    pub fingerprint: String,
    pub bound: BoundOptions,
    pub query: OptionsQuery,
    pub target: CellRunTarget,
}

/// A change waiting for Apply: the value, and what is stored for it (a
/// `{"preset": …}` object keeps a relative date relative).
#[derive(Clone, Debug, PartialEq)]
struct Pending {
    value: Option<ParamValue>,
    stored: Option<Value>,
}

#[derive(Default)]
struct OptionsSlot {
    fingerprint: Option<String>,
    state: OptionsState,
    page: OptionsPage,
    /// Reset a value that is no longer an option once this load lands
    /// (a linked parameter changed).
    reset_invalid: bool,
    /// Held back until the notebook is trusted.
    waiting_for_trust: bool,
}

pub struct ParamsState {
    loaded_for: Option<String>,
    personal: PersonalParams,
    widgets: HashMap<(Surface, String), Entity<ParamWidget>>,
    subscriptions: HashMap<(Surface, String), Subscription>,
    errors: HashMap<String, ParamError>,
    drafts: BTreeMap<String, Pending>,
    /// Remote cells a reactive change will re-run once applied.
    pending_runs: Vec<String>,
    options: HashMap<String, OptionsSlot>,
    cache: VecDeque<(String, OptionsPage)>,
    search: HashMap<String, String>,
    debounce: HashMap<String, Task<()>>,
    /// The typed edit each debounce timer above will take, so Enter or
    /// Apply can take it at once.
    typing: HashMap<String, (Edit, Surface)>,
    editor: Option<(Entity<ParamEditor>, Subscription)>,
    /// The parameter cell a new definition from the editor is bound to.
    editor_for_cell: Option<String>,
    pub(in crate::notebook) secrets: Rc<dyn SecretCatalog>,
    clock: Rc<dyn Clock>,
    /// The output registry generation the widgets last saw.
    outputs_seen: u64,
}

impl ParamsState {
    /// Drop every widget so the next render rebuilds them from the
    /// definitions (a structural undo or redo replaced them).
    pub(in crate::notebook) fn reset_widgets(&mut self) {
        self.widgets.clear();
        self.subscriptions.clear();
    }
}

impl Default for ParamsState {
    fn default() -> Self {
        Self {
            loaded_for: None,
            personal: PersonalParams::default(),
            widgets: HashMap::new(),
            subscriptions: HashMap::new(),
            errors: HashMap::new(),
            drafts: BTreeMap::new(),
            pending_runs: Vec::new(),
            options: HashMap::new(),
            cache: VecDeque::new(),
            search: HashMap::new(),
            debounce: HashMap::new(),
            typing: HashMap::new(),
            editor: None,
            editor_for_cell: None,
            secrets: Rc::new(KeyringSecrets),
            clock: Rc::new(SystemClock),
            outputs_seen: 0,
        }
    }
}

impl ParamsState {
    /// Whether the Apply bar has anything to show.
    pub fn has_pending(&self) -> bool {
        !self.drafts.is_empty() || !self.pending_runs.is_empty()
    }

    pub fn pending_names(&self) -> Vec<String> {
        self.drafts.keys().cloned().collect()
    }

    pub fn editor(&self) -> Option<&Entity<ParamEditor>> {
        self.editor.as_ref().map(|(e, _)| e)
    }

    pub fn widget(&self, surface: &Surface, name: &str) -> Option<&Entity<ParamWidget>> {
        self.widgets.get(&(surface.clone(), name.to_string()))
    }

    fn cache_get(&self, fingerprint: &str) -> Option<OptionsPage> {
        self.cache
            .iter()
            .find(|(fp, _)| fp == fingerprint)
            .map(|(_, page)| page.clone())
    }

    fn cache_put(&mut self, fingerprint: String, page: OptionsPage) {
        self.cache.retain(|(fp, _)| *fp != fingerprint);
        self.cache.push_back((fingerprint, page));
        while self.cache.len() > OPTIONS_CACHE {
            self.cache.pop_front();
        }
    }
}

/// Whether two definitions draw the same widget: a new default value does
/// not rebuild the control the person is using.
fn same_shape(a: &ParameterDef, b: &ParameterDef) -> bool {
    let strip = |d: &ParameterDef| ParameterDef {
        default: None,
        ..d.clone()
    };
    strip(a) == strip(b)
}

/// The cell a parameter reads its options from, as the columns and limit
/// to read them with.
fn from_cell(def: &ParameterDef) -> Option<(String, OptionsQuery)> {
    match &def.allowed_values {
        Some(AllowedValues::FromCell {
            from_cell,
            value_column,
            label_column,
            limit,
        }) => {
            let mut query = OptionsQuery::new(String::new());
            query.value_column = Some(value_column.clone()).filter(|c| !c.is_empty());
            query.label_column = label_column.clone();
            query.limit = *limit;
            Some((from_cell.clone(), query))
        }
        _ => None,
    }
}

/// The parameter a cell shows, if it is a parameter cell.
pub fn cell_parameter(cell: &sqlly_services::notebook::Cell) -> Option<String> {
    (cell.kind == CellKind::Parameter)
        .then(|| ParameterCellSpec::of(cell).parameter)
        .filter(|p| !p.is_empty())
}

impl NotebookView {
    fn param_def(&self, name: &str) -> Option<ParameterDef> {
        self.doc
            .param_defs()
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name))
            .cloned()
    }

    /// Set parameter `name` from typed text, as its input would (empty text
    /// clears it).
    /// Callers that gate changes themselves (the dashboard filter bar)
    /// use this, so it never waits for Apply.
    pub(crate) fn set_param(&mut self, name: &str, raw: &str, cx: &mut Context<Self>) {
        let Some(def) = self.param_def(name) else {
            return;
        };
        let parsed = if raw.trim().is_empty() {
            Ok(None)
        } else {
            params::parse_text(&def, raw).and_then(|v| params::validate(&def, &v).map(|()| Some(v)))
        };
        match parsed {
            Ok(value) => {
                self.param_ui.errors.remove(&def.name);
                self.param_ui.drafts.remove(&def.name);
                let stored = value.as_ref().map(ParamValue::to_json);
                if self.store_param_value(&def, value, stored, cx) {
                    self.after_param_change(vec![def.name.clone()], cx);
                }
            }
            Err(error) => {
                self.param_ui.errors.insert(def.name.clone(), error);
            }
        }
        self.sync_param_widgets(&def.name, None, cx);
        cx.notify();
    }

    /// The message shown under parameter `name`'s input, if any.
    pub(crate) fn param_error(&self, name: &str) -> Option<String> {
        let def = self.param_def(name)?;
        self.param_ui
            .errors
            .get(&def.name)
            .map(message)
            .or_else(|| {
                self.doc
                    .resolved_params()
                    .error(&def.name)
                    .filter(|e| **e != ParamError::Missing || def.validation.required)
                    .map(message)
            })
    }

    /// Use `clock` for presets and relative defaults.
    #[cfg(test)]
    pub fn set_param_clock(&mut self, clock: Rc<dyn Clock>) {
        self.param_ui.clock = clock.clone();
        self.doc.set_clock(clock);
    }

    /// Load personal values once per notebook and make sure every parameter
    /// has its rail widget and every parameter cell its cell widget.
    pub(in crate::notebook) fn ensure_param_widgets(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.doc.id().to_string();
        if self.param_ui.loaded_for.as_deref() != Some(id.as_str()) {
            self.param_ui.loaded_for = Some(id.clone());
            self.param_ui.personal = store::load_personal(&id);
            self.doc.set_clock(self.param_ui.clock.clone());
            let values = self
                .param_ui
                .personal
                .values_for(self.doc.param_defs(), self.param_ui.clock.as_ref());
            for (name, value) in values {
                self.doc.set_supplied(&name, Some(value));
            }
            self.doc.refresh_staleness();
        }
        let defs: Vec<ParameterDef> = self.doc.param_defs().to_vec();
        let mut wanted: Vec<(Surface, ParameterDef)> =
            defs.iter().map(|d| (Surface::Rail, d.clone())).collect();
        for cell in &self.doc.model().cells {
            if let Some(name) = cell_parameter(cell)
                && let Some(def) = defs.iter().find(|d| d.name.eq_ignore_ascii_case(&name))
            {
                wanted.push((Surface::Cell(cell.id.clone()), def.clone()));
            }
        }
        for name in self.dashboard_filter_names() {
            if let Some(def) = defs.iter().find(|d| d.name == name) {
                wanted.push((Surface::Dashboard, def.clone()));
            }
        }
        let keep: Vec<(Surface, String)> = wanted
            .iter()
            .map(|(s, d)| (s.clone(), d.name.clone()))
            .collect();
        let stale: Vec<(Surface, String)> = self
            .param_ui
            .widgets
            .iter()
            .filter(|(key, widget)| {
                !keep.contains(key)
                    || wanted
                        .iter()
                        .find(|(s, d)| s == &key.0 && d.name == key.1)
                        .is_some_and(|(_, d)| !same_shape(widget.read(cx).def(), d))
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            self.param_ui.widgets.remove(&key);
            self.param_ui.subscriptions.remove(&key);
        }
        let mut created: Vec<String> = Vec::new();
        for (surface, def) in wanted {
            let key = (surface.clone(), def.name.clone());
            if self.param_ui.widgets.contains_key(&key) {
                continue;
            }
            let name = def.name.clone();
            let compact = surface == Surface::Dashboard;
            let widget = cx.new(|cx| {
                let widget = ParamWidget::new(def, window, cx);
                if compact { widget.compact() } else { widget }
            });
            let from = surface.clone();
            let sub = cx.subscribe_in(
                &widget,
                window,
                move |this, _, event: &ParamWidgetEvent, window, cx| {
                    this.on_param_widget_event(&name, &from, event.clone(), window, cx)
                },
            );
            self.param_ui.widgets.insert(key.clone(), widget);
            self.param_ui.subscriptions.insert(key.clone(), sub);
            if !created.contains(&key.1) {
                created.push(key.1);
            }
        }
        for name in created {
            if self
                .param_def(&name)
                .and_then(|d| OptionsQuery::of(&d))
                .is_some()
                && !self.param_ui.options.contains_key(&name)
            {
                self.request_options(&name, false, false, cx);
            }
            self.sync_param_widgets(&name, None, cx);
        }
        let generation = self.outputs.generation();
        if generation != self.param_ui.outputs_seen {
            self.param_ui.outputs_seen = generation;
            for def in defs.iter().filter(|d| from_cell(d).is_some()) {
                self.sync_param_widgets(&def.name, None, cx);
            }
        }
        if self.live_trust().level != TrustLevel::Untrusted {
            let waiting: Vec<String> = defs
                .iter()
                .filter(|d| {
                    self.param_ui
                        .options
                        .get(&d.name)
                        .is_some_and(|slot| slot.waiting_for_trust)
                })
                .map(|d| d.name.clone())
                .collect();
            for name in waiting {
                self.request_options(&name, true, false, cx);
            }
        }
    }

    /// The widget a parameter cell shows (made on first use).
    pub(in crate::notebook) fn param_cell_widget(
        &mut self,
        cell_id: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ParamWidget>> {
        let key = (Surface::Cell(cell_id.to_string()), name.to_string());
        if !self.param_ui.widgets.contains_key(&key) {
            self.ensure_param_widgets(window, cx);
        }
        self.param_ui
            .widgets
            .get(&key)
            .or_else(|| {
                self.param_def(name).and_then(|d| {
                    self.param_ui
                        .widgets
                        .get(&(Surface::Cell(cell_id.to_string()), d.name))
                })
            })
            .cloned()
    }

    /// The value a parameter shows: its pending draft, else its current one.
    fn shown_value(&self, name: &str) -> Option<ParamValue> {
        match self.param_ui.drafts.get(name) {
            Some(pending) => pending.value.clone(),
            None => self.doc.resolved_params().get(name).cloned(),
        }
    }

    fn param_options(&self, def: &ParameterDef) -> (Vec<OptionItem>, OptionsState) {
        if def.ty == ParamType::SecretRef {
            let names = self.param_ui.secrets.names();
            return (
                names
                    .into_iter()
                    .map(|n| OptionItem {
                        value: Value::String(n.clone()),
                        label: n,
                    })
                    .collect(),
                OptionsState::Static,
            );
        }
        if let Some((cell, query)) = from_cell(def) {
            return self.cell_options(&cell, &query);
        }
        match self.param_ui.options.get(&def.name) {
            Some(slot) if OptionsQuery::of(def).is_some() => {
                (slot.page.items.clone(), slot.state.clone())
            }
            _ => (spec::static_options(def), OptionsState::Static),
        }
    }

    /// Options read from another cell's current output.
    fn cell_options(&self, cell: &str, query: &OptionsQuery) -> (Vec<OptionItem>, OptionsState) {
        let model = self.doc.model();
        let Some(output) = model
            .cell(cell)
            .or_else(|| model.resolve(cell))
            .and_then(|c| self.outputs.get(&c.id))
        else {
            return (Vec::new(), OptionsState::Static);
        };
        let names: Vec<String> = output.columns.iter().map(|c| c.name.clone()).collect();
        match spec::OptionsCollector::new(&names, query) {
            Ok(mut collector) => {
                for row in output.rows.iter() {
                    if collector.push(row) {
                        break;
                    }
                }
                let page = collector.finish();
                let truncated = page.truncated;
                (page.items, OptionsState::Loaded { truncated })
            }
            Err(error) => (
                Vec::new(),
                OptionsState::Failed(crate::localization::message(error.message())),
            ),
        }
    }

    fn menu_entries(&self, def: &ParameterDef) -> (Vec<MenuEntry>, Vec<MenuEntry>) {
        let mut presets: Vec<MenuEntry> = editor::offered_date_presets(def)
            .into_iter()
            .map(|p| MenuEntry {
                pick: PresetPick::Date(p),
                label: t(editor::date_preset_label_key(p)),
            })
            .collect();
        for preset in &def.presets {
            if let Preset::Named(named) = preset {
                presets.push(MenuEntry {
                    pick: PresetPick::Named(named.id.clone()),
                    label: if named.label.is_empty() {
                        named.id.clone()
                    } else {
                        named.label.clone()
                    },
                });
            }
        }
        let sets = self
            .param_sets()
            .into_iter()
            .map(|(set, personal)| MenuEntry {
                label: crate::localization::format(
                    if personal {
                        "notebook.params.presets.apply_personal_set"
                    } else {
                        "notebook.params.presets.apply_shared_set"
                    },
                    &[("set_name", set.name.as_str())],
                ),
                pick: PresetPick::ApplySet {
                    id: set.id,
                    personal,
                },
            })
            .collect();
        (presets, sets)
    }

    /// Every named set: the notebook's, then this person's.
    pub(in crate::notebook) fn param_sets(&self) -> Vec<(spec::ParameterSet, bool)> {
        spec::shared_sets(self.doc.model())
            .into_iter()
            .map(|s| (s, false))
            .chain(
                self.param_ui
                    .personal
                    .sets
                    .iter()
                    .cloned()
                    .map(|s| (s, true)),
            )
            .collect()
    }

    fn widget_status(&self, def: &ParameterDef) -> WidgetStatus {
        let resolved = self.doc.resolved_params();
        let error = self
            .param_ui
            .errors
            .get(&def.name)
            .map(message)
            .or_else(|| {
                resolved
                    .error(&def.name)
                    .filter(|e| **e != ParamError::Missing || def.validation.required)
                    .map(message)
            });
        let (options, options_state) = self.param_options(def);
        let (presets, sets) = self.menu_entries(def);
        let query = OptionsQuery::of(def);
        WidgetStatus {
            value: self.shown_value(&def.name),
            error,
            pending: self.param_ui.drafts.contains_key(&def.name),
            options,
            options_state,
            has_query: query.is_some(),
            searches: query.is_some_and(|q| q.searches(self.doc.database_type())),
            presets,
            sets,
        }
    }

    /// Push `name`'s state into its widgets; `from` (the widget the person
    /// is using) keeps what it shows.
    fn sync_param_widgets(&mut self, name: &str, from: Option<&Surface>, cx: &mut Context<Self>) {
        let Some(def) = self.param_def(name) else {
            return;
        };
        let status = self.widget_status(&def);
        let widgets: Vec<(Surface, Entity<ParamWidget>)> = self
            .param_ui
            .widgets
            .iter()
            .filter(|((_, n), _)| *n == def.name)
            .map(|((s, _), w)| (s.clone(), w.clone()))
            .collect();
        for (surface, widget) in widgets {
            let external = from != Some(&surface);
            let status = status.clone();
            widget.update(cx, |w, cx| w.sync(status, external, cx));
        }
    }

    fn sync_all_param_widgets(&mut self, cx: &mut Context<Self>) {
        let names: Vec<String> = self
            .doc
            .param_defs()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        for name in names {
            self.sync_param_widgets(&name, None, cx);
        }
    }

    fn on_param_widget_event(
        &mut self,
        name: &str,
        from: &Surface,
        event: ParamWidgetEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ParamWidgetEvent::Edited { edit, debounce } => {
                if debounce {
                    let key = name.to_string();
                    let name = key.clone();
                    self.param_ui
                        .typing
                        .insert(key.clone(), (edit, from.clone()));
                    let task = cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(PARAM_DEBOUNCE).await;
                        this.update(cx, |this, cx| this.take_param_typing(&name, cx))
                            .ok();
                    });
                    self.param_ui.debounce.insert(key, task);
                } else {
                    self.param_ui.debounce.remove(name);
                    self.param_ui.typing.remove(name);
                    self.edit_param(name, edit, Some(from), cx);
                }
            }
            ParamWidgetEvent::Submit => {
                self.take_param_typing(name, cx);
                if *from == Surface::Dashboard {
                    self.apply_filters(cx);
                }
            }
            ParamWidgetEvent::Preset(pick) => self.apply_param_preset(name, pick, cx),
            ParamWidgetEvent::RefreshOptions => self.request_options(name, true, false, cx),
            ParamWidgetEvent::Search(text) => {
                self.param_ui.search.insert(name.to_string(), text);
                let searches = self
                    .param_def(name)
                    .and_then(|d| OptionsQuery::of(&d))
                    .is_some_and(|q| q.searches(self.doc.database_type()));
                if searches {
                    let key = format!("search:{name}");
                    let (name, done) = (name.to_string(), key.clone());
                    let task = cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(PARAM_DEBOUNCE).await;
                        this.update(cx, |this, cx| {
                            this.param_ui.debounce.remove(&done);
                            this.request_options(&name, false, false, cx);
                        })
                        .ok();
                    });
                    self.param_ui.debounce.insert(key, task);
                }
            }
            ParamWidgetEvent::EditDefinition => {
                self.open_param_editor(Some(name.to_string()), window, cx)
            }
            ParamWidgetEvent::AddSecret {
                name: secret,
                value,
            } => match self.param_ui.secrets.add(&secret, &value) {
                Ok(()) => {
                    self.edit_param(name, Edit::Choice(vec![Value::String(secret)]), None, cx)
                }
                Err(error) => self.status(error, true, cx),
            },
        }
    }

    /// Take what the person did to `name`: parse and validate it, then hold
    /// it for Apply or store it.
    pub(in crate::notebook) fn edit_param(
        &mut self,
        name: &str,
        edit: Edit,
        from: Option<&Surface>,
        cx: &mut Context<Self>,
    ) {
        let Some(def) = self.param_def(name) else {
            return;
        };
        let parsed = match edit {
            Edit::Text(text) => params::parse_text(&def, &text).map(Some),
            Edit::Value(value) => Ok(Some(value)),
            Edit::Choice(values) => spec::choice_value(&def, &values).map(Some),
            Edit::Clear => Ok(None),
        }
        .and_then(|value| match &value {
            Some(v) => params::validate(&def, v).map(|()| value),
            None if def.validation.required => Err(ParamError::Required),
            None => Ok(None),
        })
        .and_then(|value| match &value {
            Some(v) if !self.is_param_option(&def, v) => Err(ParamError::NotAllowed),
            _ => Ok(value),
        });
        match parsed {
            Ok(value) => {
                self.param_ui.errors.remove(&def.name);
                let stored = value.as_ref().map(ParamValue::to_json);
                self.commit_param_value(&def, value, stored, from, cx);
            }
            Err(error) => {
                self.param_ui.errors.insert(def.name.clone(), error);
                self.sync_param_widgets(&def.name, from, cx);
                cx.notify();
            }
        }
    }

    /// Whether `value` is among `def`'s query-filled options, once loaded.
    fn is_param_option(&self, def: &ParameterDef, value: &ParamValue) -> bool {
        if OptionsQuery::of(def).is_none()
            || !matches!(def.ty, ParamType::Enum | ParamType::MultiSelect)
        {
            return true;
        }
        let Some(slot) = self.param_ui.options.get(&def.name) else {
            return true;
        };
        if !matches!(slot.state, OptionsState::Loaded { .. }) {
            return true;
        }
        let allowed: Vec<String> = slot
            .page
            .items
            .iter()
            .map(|o| spec::value_text(&o.value))
            .collect();
        let items: Vec<&ParamValue> = match value {
            ParamValue::List(items) => items.iter().collect(),
            ParamValue::Null => Vec::new(),
            single => vec![single],
        };
        items
            .iter()
            .all(|v| allowed.contains(&spec::value_text(&v.to_json())))
    }

    fn commit_param_value(
        &mut self,
        def: &ParameterDef,
        value: Option<ParamValue>,
        stored: Option<Value>,
        from: Option<&Surface>,
        cx: &mut Context<Self>,
    ) {
        if def.reactivity.waits_for_apply() {
            let current = self.doc.resolved_params().get(&def.name).cloned();
            if current == value {
                self.param_ui.drafts.remove(&def.name);
            } else {
                self.param_ui
                    .drafts
                    .insert(def.name.clone(), Pending { value, stored });
            }
            self.sync_param_widgets(&def.name, from, cx);
            cx.notify();
            return;
        }
        if self.store_param_value(def, value, stored, cx) {
            self.after_param_change(vec![def.name.clone()], cx);
        }
        self.sync_param_widgets(&def.name, from, cx);
        cx.notify();
    }

    /// Keep a value where its parameter keeps values. Returns whether the
    /// value in use changed.
    fn store_param_value(
        &mut self,
        def: &ParameterDef,
        value: Option<ParamValue>,
        stored: Option<Value>,
        cx: &mut Context<Self>,
    ) -> bool {
        if def.sensitive || PersonalParams::keeps(def) {
            if PersonalParams::keeps(def) {
                let kept = stored.as_ref().filter(|v| v.is_object()).cloned();
                let changed = match kept {
                    Some(preset) => {
                        let before = self
                            .param_ui
                            .personal
                            .values
                            .insert(def.name.clone(), preset.clone());
                        before.as_ref() != Some(&preset)
                    }
                    None => self.param_ui.personal.set_value(def, value.as_ref()),
                };
                if changed {
                    self.save_personal_params();
                }
            }
            return self.doc.set_supplied(&def.name, value);
        }
        let before = self.doc.resolved_params().get(&def.name).cloned();
        self.doc.set_supplied(&def.name, None);
        let name = def.name.clone();
        let wrote = self.doc.edit_params(|doc| {
            let Some(p) = doc.parameters.iter_mut().find(|p| p.name == name) else {
                return false;
            };
            if p.default == stored {
                return false;
            }
            p.default = stored;
            true
        });
        if wrote {
            self.mark_dirty(cx);
        }
        self.doc.resolved_params().get(&def.name).cloned() != before
    }

    fn save_personal_params(&mut self) {
        let personal = self.param_ui.personal.clone();
        if let Err(error) = store::save_personal(&personal) {
            tracing::warn!("notebook personal parameters not saved: {error}");
        }
    }

    /// After values in use changed: staleness, linked options, Markdown,
    /// and in reactive mode the re-runs.
    fn after_param_change(&mut self, changed: Vec<String>, cx: &mut Context<Self>) {
        self.doc.refresh_staleness();
        self.dashboard_filters_changed(&changed);
        let db = self.doc.database_type();
        for linked in spec::linked_dependents(db, self.doc.param_defs(), &changed) {
            self.request_options(&linked, false, true, cx);
        }
        self.remeasure_param_text(&changed);
        if self.doc.run_mode() == RunMode::Reactive {
            let plan = self.doc.reactive_plan(&changed);
            for id in plan.after_apply {
                if !self.param_ui.pending_runs.contains(&id) {
                    self.param_ui.pending_runs.push(id);
                }
            }
            if !plan.run_now.is_empty() {
                self.run_param_cells(plan.run_now, cx);
            }
        }
    }

    /// Markdown cells that show a changed value change height.
    fn remeasure_param_text(&mut self, changed: &[String]) {
        let ids: Vec<String> = {
            let graph = self.doc.graph();
            changed
                .iter()
                .flat_map(|name| graph.downstream_closure(&Node::param(name.clone())))
                .filter_map(|n| n.as_cell().map(str::to_string))
                .collect()
        };
        for id in ids {
            if self
                .doc
                .cell_by_id(&id)
                .is_some_and(|c| c.kind == "markdown")
            {
                self.remeasure_cell(&id);
            }
        }
    }

    fn run_param_cells(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| self.doc.cell_by_id(id).is_some())
            .collect();
        if ids.is_empty() {
            return;
        }
        let selection = std::mem::replace(&mut self.selected, ids);
        self.run_scope(RunScope::Selected, None, cx);
        self.selected = selection;
    }

    /// The cells that would re-run (reactive) or go out of date (manual)
    /// when the pending changes are applied.
    pub(in crate::notebook) fn param_apply_preview(&self) -> Vec<String> {
        let names = self.param_ui.pending_names();
        let plan = self.doc.reactive_plan(&names);
        let mut out = self.param_ui.pending_runs.clone();
        for id in plan.after_apply.into_iter().chain(plan.run_now) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// Store every pending change and, in reactive mode, run what they and
    /// earlier changes invalidated.
    pub fn apply_params(&mut self, cx: &mut Context<Self>) {
        let drafts = std::mem::take(&mut self.param_ui.drafts);
        let mut changed = Vec::new();
        for (name, pending) in drafts {
            if let Some(def) = self.param_def(&name)
                && self.store_param_value(&def, pending.value, pending.stored, cx)
            {
                changed.push(def.name.clone());
            }
        }
        let mut runs = std::mem::take(&mut self.param_ui.pending_runs);
        if !changed.is_empty() {
            self.doc.refresh_staleness();
            let db = self.doc.database_type();
            for linked in spec::linked_dependents(db, self.doc.param_defs(), &changed) {
                self.request_options(&linked, false, true, cx);
            }
            self.remeasure_param_text(&changed);
        }
        if self.doc.run_mode() == RunMode::Reactive {
            let plan = self.doc.reactive_plan(&changed);
            for id in plan.after_apply.into_iter().chain(plan.run_now) {
                if !runs.contains(&id) {
                    runs.push(id);
                }
            }
            let order: Vec<String> = self.doc.cells().map(|c| c.id.to_string()).collect();
            runs.sort_by_key(|id| order.iter().position(|o| o == id));
            self.run_param_cells(runs, cx);
        }
        if !changed.is_empty() {
            let applied = crate::localization::format(
                if changed.len() == 1 {
                    "notebook.params.apply.applied_one"
                } else {
                    "notebook.params.apply.applied"
                },
                &[("count", changed.len())],
            );
            self.log(None, applied, false, None);
        }
        self.sync_all_param_widgets(cx);
        cx.notify();
    }

    /// Whether parameter `name` has a change waiting for Apply.
    pub(in crate::notebook) fn has_param_draft(&self, name: &str) -> bool {
        self.param_ui.drafts.contains_key(name)
    }

    /// Take what was typed into `name` now instead of when its debounce
    /// timer fires.
    pub(in crate::notebook) fn take_param_typing(&mut self, name: &str, cx: &mut Context<Self>) {
        self.param_ui.debounce.remove(name);
        if let Some((edit, from)) = self.param_ui.typing.remove(name) {
            self.edit_param(name, edit, Some(&from), cx);
        }
    }

    /// Whether `name` has typing its debounce timer hasn't taken yet.
    pub(in crate::notebook) fn has_param_typing(&self, name: &str) -> bool {
        self.param_ui.typing.contains_key(name)
    }

    /// Store the pending changes of `names` only (a dashboard's Apply);
    /// other drafts keep waiting.
    pub(in crate::notebook) fn apply_param_drafts(
        &mut self,
        names: &[String],
        cx: &mut Context<Self>,
    ) {
        let mut changed = Vec::new();
        for name in names {
            if let Some(pending) = self.param_ui.drafts.remove(name)
                && let Some(def) = self.param_def(name)
                && self.store_param_value(&def, pending.value, pending.stored, cx)
            {
                changed.push(def.name.clone());
            }
        }
        if !changed.is_empty() {
            self.after_param_change(changed, cx);
        }
        self.sync_all_param_widgets(cx);
        cx.notify();
    }

    /// Drop pending changes and forget queued re-runs.
    pub fn discard_params(&mut self, cx: &mut Context<Self>) {
        self.param_ui.drafts.clear();
        self.param_ui.typing.clear();
        self.param_ui
            .debounce
            .retain(|key, _| key.starts_with("search:"));
        self.param_ui.pending_runs.clear();
        self.param_ui.errors.clear();
        self.sync_all_param_widgets(cx);
        cx.notify();
    }

    pub(in crate::notebook) fn set_reactive(&mut self, on: bool, cx: &mut Context<Self>) {
        let mode = if on {
            RunMode::Reactive
        } else {
            RunMode::Manual
        };
        if self.doc.set_run_mode(mode) {
            if !on {
                self.param_ui.pending_runs.clear();
            }
            self.mark_dirty(cx);
        }
    }

    fn apply_param_preset(&mut self, name: &str, pick: PresetPick, cx: &mut Context<Self>) {
        let Some(def) = self.param_def(name) else {
            return;
        };
        let clock = self.param_ui.clock.clone();
        match pick {
            PresetPick::Date(preset) => {
                match params::resolve_preset(&def, &Preset::Date(preset), clock.as_ref()) {
                    Ok(value) => {
                        self.param_ui.errors.remove(&def.name);
                        let stored =
                            serde_json::json!({ "preset": editor::date_preset_id(preset) });
                        self.commit_param_value(&def, Some(value), Some(stored), None, cx);
                    }
                    Err(error) => self.status(message(&error), true, cx),
                }
            }
            PresetPick::Named(id) => {
                let found = params::find_preset(&def, &id)
                    .map(|p| params::resolve_preset(&def, &p, clock.as_ref()));
                match found {
                    Some(Ok(value)) => {
                        self.param_ui.errors.remove(&def.name);
                        let stored = Some(value.to_json());
                        self.commit_param_value(&def, Some(value), stored, None, cx);
                    }
                    Some(Err(error)) => self.status(message(&error), true, cx),
                    None => {}
                }
            }
            PresetPick::ApplySet { id, personal } => self.apply_param_set(&id, personal, cx),
            PresetPick::SaveSet { name, personal } => self.save_param_set(&name, personal, cx),
        }
    }

    pub(in crate::notebook) fn apply_param_set(
        &mut self,
        id: &str,
        personal: bool,
        cx: &mut Context<Self>,
    ) {
        let Some((set, _)) = self
            .param_sets()
            .into_iter()
            .find(|(s, p)| s.id == id && *p == personal)
        else {
            return;
        };
        let (values, errors) =
            spec::set_values(self.doc.param_defs(), &set, self.param_ui.clock.as_ref());
        let mut changed = Vec::new();
        for (name, value) in values {
            let Some(def) = self.param_def(&name) else {
                continue;
            };
            self.param_ui.errors.remove(&def.name);
            let stored = set.values.get(&name).cloned();
            if def.reactivity.waits_for_apply() {
                self.commit_param_value(&def, Some(value), stored, None, cx);
            } else if self.store_param_value(&def, Some(value), stored, cx) {
                changed.push(def.name.clone());
            }
        }
        if !changed.is_empty() {
            self.after_param_change(changed, cx);
        }
        if let Some((name, error)) = errors.first() {
            self.status(
                crate::localization::format(
                    "notebook.params.presets.set_value_skipped",
                    &[
                        ("parameter_name", name.as_str()),
                        ("error", message(error).as_str()),
                    ],
                ),
                true,
                cx,
            );
        }
        self.sync_all_param_widgets(cx);
        cx.notify();
    }

    fn effective_param_values(&self) -> BTreeMap<String, ParamValue> {
        let mut values = self.doc.resolved_params().values;
        for (name, pending) in &self.param_ui.drafts {
            match &pending.value {
                Some(v) => values.insert(name.clone(), v.clone()),
                None => values.remove(name),
            };
        }
        values
    }

    pub(in crate::notebook) fn save_param_set(
        &mut self,
        name: &str,
        personal: bool,
        cx: &mut Context<Self>,
    ) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let id = format!("set_{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
        let target = if personal {
            SetTarget::Personal
        } else {
            SetTarget::Shared
        };
        let set = spec::capture_set(
            id,
            name,
            self.doc.param_defs(),
            &self.effective_param_values(),
            target,
        );
        if personal {
            self.param_ui.personal.sets.push(set);
            self.save_personal_params();
        } else {
            self.doc.edit_params(|doc| {
                let mut sets = spec::shared_sets(doc);
                sets.push(set);
                spec::store_shared_sets(doc, &sets);
            });
            self.mark_dirty(cx);
        }
        self.status(
            crate::localization::format("notebook.params.presets.saved", &[("set_name", name)]),
            false,
            cx,
        );
        self.sync_all_param_widgets(cx);
    }

    pub(in crate::notebook) fn delete_param_set(
        &mut self,
        id: &str,
        personal: bool,
        cx: &mut Context<Self>,
    ) {
        if personal {
            self.param_ui.personal.sets.retain(|s| s.id != id);
            self.save_personal_params();
        } else {
            let id = id.to_string();
            self.doc.edit_params(|doc| {
                let mut sets = spec::shared_sets(doc);
                sets.retain(|s| s.id != id);
                spec::store_shared_sets(doc, &sets);
            });
            self.mark_dirty(cx);
        }
        self.sync_all_param_widgets(cx);
        cx.notify();
    }

    /// Where `query` runs: its slot (or the notebook's default connection,
    /// else the tab's), always read-only; and the connection part of its
    /// cache key.
    fn options_target(&self, query: &OptionsQuery) -> Result<(CellRunTarget, String), String> {
        let refs = &self.doc.model().connection_refs;
        let slot = match &query.connection_ref {
            Some(id) => Some(refs.iter().find(|r| &r.id == id).ok_or_else(|| {
                crate::localization::format(
                    "notebook.params.options.unknown_connection",
                    &[("connection_name", id.as_str())],
                )
            })?),
            None => self.doc.notebook_connection(),
        };
        let target = |profile_id| CellRunTarget {
            profile_id,
            read_only: true,
            timeout_seconds: Some(OPTIONS_TIMEOUT_SECONDS),
            local: false,
        };
        match slot {
            Some(slot) => {
                let unresolved = || {
                    crate::localization::format(
                        "notebook.shell.remap.unresolved_run",
                        &[("connection_name", slot.label.as_str())],
                    )
                };
                let profile = slot.profile_id.as_deref().ok_or_else(unresolved)?;
                if !self.is_resolved(profile) {
                    return Err(unresolved());
                }
                let uuid = uuid::Uuid::parse_str(profile).map_err(|_| unresolved())?;
                let key = format!("{profile}:{}", slot.database.as_deref().unwrap_or_default());
                Ok((target(Some(uuid)), key))
            }
            None => {
                let key = self
                    .tab_connection
                    .as_ref()
                    .map(|c| {
                        format!(
                            "{}:{}",
                            c.profile_id,
                            c.database.as_deref().unwrap_or_default()
                        )
                    })
                    .unwrap_or_default();
                Ok((target(None), key))
            }
        }
    }

    /// Whether trust lets `query` run: the notebook is trusted and the
    /// connection it runs on is one the trust covers.
    fn options_allowed(&self, query: &OptionsQuery) -> bool {
        let trust = self.live_trust();
        if trust.level == TrustLevel::Untrusted {
            return false;
        }
        let slot = query
            .connection_ref
            .as_deref()
            .or_else(|| self.doc.notebook_connection().map(|s| s.id.as_str()));
        match slot {
            Some(id) => {
                let profile = self
                    .doc
                    .model()
                    .connection_refs
                    .iter()
                    .find(|r| r.id == id)
                    .and_then(|r| r.profile_id.as_deref());
                trust.authorizes(id, profile)
            }
            None => true,
        }
    }

    fn options_db(&self, query: &OptionsQuery) -> sqlly_lang::sqlmodel::DatabaseType {
        let refs = &self.doc.model().connection_refs;
        let slot = match &query.connection_ref {
            Some(id) => refs.iter().find(|r| &r.id == id),
            None => self.doc.notebook_connection(),
        };
        slot.and_then(|s| s.profile_id.as_deref())
            .and_then(|p| self.choices.iter().find(|c| c.profile_id == p))
            .map(|c| c.database_type)
            .unwrap_or_else(|| self.doc.database_type())
    }

    /// Read `name`'s options again: from the cache when its fingerprint is
    /// known (unless `force`), else by asking the host to run its discovery
    /// query. `reset_invalid` drops a value that is no longer an option.
    pub(in crate::notebook) fn request_options(
        &mut self,
        name: &str,
        force: bool,
        reset_invalid: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(def) = self.param_def(name) else {
            return;
        };
        let Some(query) = OptionsQuery::of(&def) else {
            self.param_ui.options.remove(&def.name);
            return;
        };
        let allowed = self.options_allowed(&query);
        let slot = self.param_ui.options.entry(def.name.clone()).or_default();
        slot.reset_invalid |= reset_invalid;
        if !allowed {
            slot.waiting_for_trust = true;
            slot.state = OptionsState::Failed(t("notebook.params.options.untrusted"));
            self.sync_param_widgets(&def.name, None, cx);
            return;
        }
        slot.waiting_for_trust = false;
        let search = self
            .param_ui
            .search
            .get(&def.name)
            .cloned()
            .unwrap_or_default();
        let db = self.options_db(&query);
        let resolved = self.doc.resolved_params();
        let bound = spec::bind_options(db, &def, &query, self.doc.param_defs(), &resolved, &search)
            .map_err(|e| crate::localization::message(e.message()))
            .and_then(|bound| self.options_target(&query).map(|t| (bound, t)));
        let slot = self.param_ui.options.entry(def.name.clone()).or_default();
        let (bound, (target, connection)) = match bound {
            Ok(ok) => ok,
            Err(error) => {
                slot.fingerprint = None;
                slot.state = OptionsState::Failed(error);
                self.sync_param_widgets(&def.name, None, cx);
                return;
            }
        };
        let fingerprint = spec::options_fingerprint(&bound, &connection);
        let cached = (!force)
            .then(|| self.param_ui.cache_get(&fingerprint))
            .flatten();
        let slot = self.param_ui.options.entry(def.name.clone()).or_default();
        if let Some(page) = cached {
            slot.fingerprint = Some(fingerprint.clone());
            self.param_options_loaded(&def.name, &fingerprint, Ok(page), cx);
            return;
        }
        if !force
            && slot.fingerprint.as_deref() == Some(fingerprint.as_str())
            && slot.state == OptionsState::Loading
        {
            return;
        }
        slot.fingerprint = Some(fingerprint.clone());
        slot.state = OptionsState::Loading;
        self.sync_param_widgets(&def.name, None, cx);
        cx.emit(NotebookEvent::ParamOptionsRequested(Box::new(
            ParamOptionsRequest {
                param: def.name.clone(),
                fingerprint,
                bound,
                query,
                target,
            },
        )));
        cx.notify();
    }

    /// The host's answer to [`NotebookEvent::ParamOptionsRequested`]. An
    /// answer for an older request is ignored.
    pub fn param_options_loaded(
        &mut self,
        param: &str,
        fingerprint: &str,
        result: Result<OptionsPage, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.param_ui.options.get_mut(param) else {
            return;
        };
        if slot.fingerprint.as_deref() != Some(fingerprint) {
            return;
        }
        let reset = std::mem::take(&mut slot.reset_invalid);
        match result {
            Ok(page) => {
                slot.state = OptionsState::Loaded {
                    truncated: page.truncated,
                };
                slot.page = page.clone();
                self.param_ui.cache_put(fingerprint.to_string(), page);
                self.revalidate_param_options(param, reset, cx);
            }
            Err(error) => slot.state = OptionsState::Failed(error),
        }
        self.sync_param_widgets(param, None, cx);
        cx.notify();
    }

    /// After options change: a value that is no longer an option is dropped
    /// when a linked parameter changed, else flagged.
    fn revalidate_param_options(&mut self, name: &str, reset: bool, cx: &mut Context<Self>) {
        let Some(def) = self.param_def(name) else {
            return;
        };
        let Some(value) = self
            .shown_value(&def.name)
            .filter(|v| *v != ParamValue::Null)
        else {
            return;
        };
        if self.is_param_option(&def, &value) {
            if self.param_ui.errors.get(&def.name) == Some(&ParamError::NotAllowed) {
                self.param_ui.errors.remove(&def.name);
            }
            return;
        }
        if !reset {
            self.param_ui
                .errors
                .insert(def.name.clone(), ParamError::NotAllowed);
            return;
        }
        let kept = match &value {
            ParamValue::List(items) => {
                let kept: Vec<ParamValue> = items
                    .iter()
                    .filter(|v| self.is_param_option(&def, &ParamValue::List(vec![(*v).clone()])))
                    .cloned()
                    .collect();
                Some(ParamValue::List(kept))
            }
            _ => None,
        };
        let stored = kept.as_ref().map(ParamValue::to_json);
        self.commit_param_value(&def, kept, stored, None, cx);
    }

    pub(in crate::notebook) fn open_param_editor(
        &mut self,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let def = name.and_then(|n| self.param_def(&n));
        let mut slots = vec![SlotChoice {
            id: None,
            label: t("notebook.params.editor.query_connection_default"),
        }];
        slots.extend(self.doc.model().connection_refs.iter().map(|r| SlotChoice {
            id: Some(r.id.clone()),
            label: r.label.clone(),
        }));
        let clock = self.param_ui.clock.clone();
        let editor = cx.new(|cx| ParamEditor::new(def, slots, clock, window, cx));
        let sub = cx.subscribe_in(
            &editor,
            window,
            |this, editor, event: &ParamEditorEvent, _, cx| {
                this.on_param_editor_event(editor.clone(), event.clone(), cx)
            },
        );
        self.param_ui.editor = Some((editor, sub));
        cx.notify();
    }

    /// Open the editor for a new parameter that parameter cell `cell_id`
    /// will show.
    pub(in crate::notebook) fn new_param_for_cell(
        &mut self,
        cell_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.param_ui.editor_for_cell = Some(cell_id.to_string());
        self.open_param_editor(None, window, cx);
    }

    /// Make parameter cell `cell_id` show parameter `name`.
    pub(in crate::notebook) fn bind_param_cell(
        &mut self,
        cell_id: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        let spec = ParameterCellSpec::for_parameter(name).to_value();
        let id = cell_id.to_string();
        let changed = self.doc.edit_params(|doc| {
            doc.cells
                .iter_mut()
                .find(|c| c.id == id)
                .filter(|c| c.spec != spec)
                .map(|c| c.spec = spec.clone())
                .is_some()
        });
        if changed {
            self.remeasure_cell(cell_id);
            self.mark_dirty(cx);
        }
    }

    fn close_param_editor(&mut self, cx: &mut Context<Self>) {
        self.param_ui.editor = None;
        self.param_ui.editor_for_cell = None;
        cx.notify();
    }

    fn on_param_editor_event(
        &mut self,
        editor: Entity<ParamEditor>,
        event: ParamEditorEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ParamEditorEvent::Cancel => self.close_param_editor(cx),
            ParamEditorEvent::Delete(name) => {
                if self.doc.remove_param(&name) {
                    self.param_ui.drafts.remove(&name);
                    self.param_ui.errors.remove(&name);
                    self.param_ui.options.remove(&name);
                    if self.param_ui.personal.values.remove(&name).is_some() {
                        self.save_personal_params();
                    }
                    self.doc.refresh_staleness();
                    self.mark_dirty(cx);
                }
                self.close_param_editor(cx);
            }
            ParamEditorEvent::Save { original, def } => {
                let def = *def;
                let name = def.name.clone();
                if let Err(error) = self.save_param_def(original.as_deref(), def, cx) {
                    editor.update(cx, |e, cx| e.set_error(Field::Name, error, cx));
                    return;
                }
                if let Some(cell) = self.param_ui.editor_for_cell.take() {
                    self.bind_param_cell(&cell, &name, cx);
                }
                self.close_param_editor(cx);
            }
        }
    }

    /// Add `def`, or replace the parameter named `original` with it
    /// (renaming every use first).
    pub(in crate::notebook) fn save_param_def(
        &mut self,
        original: Option<&str>,
        def: ParameterDef,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        match original {
            None => self.doc.add_param_def(def.clone())?,
            Some(old) => {
                if old != def.name {
                    self.doc.rename_param(old, &def.name)?;
                    self.sync_editors_to_doc(cx);
                    if let Some(v) = self.param_ui.personal.values.remove(old) {
                        self.param_ui.personal.values.insert(def.name.clone(), v);
                    }
                    self.param_ui.errors.remove(old);
                    self.param_ui.drafts.remove(old);
                    self.param_ui.options.remove(old);
                }
                let name = def.name.clone();
                let replacement = def.clone();
                self.doc.edit_params(|doc| {
                    if let Some(p) = doc.parameters.iter_mut().find(|p| p.name == name) {
                        *p = replacement;
                    }
                });
            }
        }
        let supplied = self.doc.supplied().get(&def.name).cloned();
        if let Some(value) = supplied
            && params::validate(&def, &value).is_err()
        {
            self.doc.set_supplied(&def.name, None);
        }
        if !PersonalParams::keeps(&def) && self.param_ui.personal.values.remove(&def.name).is_some()
        {
            self.save_personal_params();
        }
        if def.sensitive {
            self.param_ui.drafts.remove(&def.name);
        }
        self.param_ui.errors.remove(&def.name);
        self.param_ui.options.remove(&def.name);
        self.doc.refresh_staleness();
        self.mark_dirty(cx);
        Ok(())
    }
}
