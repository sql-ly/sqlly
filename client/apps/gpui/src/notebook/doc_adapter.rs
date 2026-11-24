//! The one door between the notebook shell and the notebook core in
//! `sqlly_services::notebook`. The shell reads cells through [`CellView`],
//! changes them through the methods here, and asks this module to plan runs
//! (the core planner), build the SQL a run sends (core expansion and
//! parameter binding), and track cell state (the core reducer, with
//! fingerprint-based staleness).

use std::cell::{Cell as StdCell, Ref, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use sqlly_core::DisplayMessage;
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::notebook::cells::attachment::AttachmentSpec;
use sqlly_services::notebook::cells::local::{self as local_spec, CacheSettings};
use sqlly_services::notebook::cells::parameter::{self as param_cells, ReactivePlan};
use sqlly_services::notebook::document::new_cell_id;
use sqlly_services::notebook::expand::substitute_relations;
use sqlly_services::notebook::fingerprint::DEFAULT_CONNECTION;
use sqlly_services::notebook::params::{self, Clock, SystemClock};
use sqlly_services::notebook::plan::StepAction;
use sqlly_services::notebook::references;
use sqlly_services::notebook::state::OutputMeta;
use sqlly_services::notebook::{
    Cell, CellEvent, CellFingerprint, CellKind, CellState, CellStatus, ConnectionIdentity,
    ConnectionRef, Dialects, ExecutionPolicy, ExpandOptions, Expansion, FingerprintInputs, Graph,
    Node, NotebookDoc, NotebookState, ParamValue, ParameterDef, PlanContext, RandomIds,
    ResolvedParams, RunMode, RunPlan, RunRequest, expand_sql, fingerprint_all,
};

use sqlly_services::analytics::scalar::ScalarValue;

use super::connection::ConnectionChoice;

pub const KIND_SQL: &str = "sql";
pub const KIND_MARKDOWN: &str = "markdown";
pub const KIND_ATTACHMENT: &str = "attachment";

/// Where a local cell's `{{x}}` input comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSource {
    /// A SQL cell on a connection: copied in from its result or its query.
    Remote,
    /// Another local SQL cell: recomputed from local data.
    LocalSql,
    /// An attachment cell's imported file.
    Attachment,
    /// A kind local data cannot read from.
    Other,
}

/// One relation a local cell reads, with the cell it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalInput {
    pub cell_id: String,
    pub relation: String,
    pub source: InputSource,
}

fn text(message: DisplayMessage) -> String {
    crate::localization::message(message)
}

/// Read-only view of one cell.
#[derive(Debug, Clone, Copy)]
pub struct CellView<'a> {
    pub id: &'a str,
    /// Stable kind id (`sql`, `markdown`, or a kind this build does not render).
    pub kind: &'a str,
    pub source: &'a str,
    pub name: Option<&'a str>,
    pub collapsed: bool,
    /// The connection slot the cell names itself (`None`: the notebook's).
    pub connection: Option<&'a ConnectionRef>,
    pub policy: &'a ExecutionPolicy,
}

impl CellView<'_> {
    pub fn is_sql(&self) -> bool {
        self.kind == KIND_SQL
    }

    /// The label the shell shows for the cell: its name, else its id.
    pub fn display_name(&self) -> &str {
        self.name.unwrap_or(self.id)
    }

    pub fn read_only(&self) -> bool {
        self.policy.read_only
    }
}

fn view_of<'a>(doc: &'a NotebookDoc, cell: &'a Cell) -> CellView<'a> {
    CellView {
        id: &cell.id,
        kind: cell.kind.as_str(),
        source: &cell.source,
        name: cell.name.as_deref(),
        collapsed: cell.collapsed,
        connection: cell
            .connection_ref
            .as_deref()
            .and_then(|s| doc.connection(s)),
        policy: &cell.execution_policy,
    }
}

/// A removed cell, kept so delete can be undone.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct RemovedCell {
    index: usize,
    cell: Cell,
}

#[cfg(test)]
impl RemovedCell {
    pub fn id(&self) -> &str {
        &self.cell.id
    }
}

/// What a run covers. `ValidateOnly` checks every cell and runs nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunScope {
    Cell,
    CellWithUpstream,
    All,
    Stale,
    Downstream,
    Above,
    Below,
    Selected,
    ValidateOnly,
}

impl RunScope {
    pub const MENU: [Self; 9] = [
        Self::Cell,
        Self::CellWithUpstream,
        Self::All,
        Self::Stale,
        Self::Downstream,
        Self::Above,
        Self::Below,
        Self::Selected,
        Self::ValidateOnly,
    ];

    pub fn label_key(self) -> &'static str {
        match self {
            Self::Cell => "notebook.shell.run.cell",
            Self::CellWithUpstream => "notebook.shell.run.cell_with_upstream",
            Self::All => "notebook.shell.run.all",
            Self::Stale => "notebook.shell.run.stale",
            Self::Downstream => "notebook.shell.run.downstream",
            Self::Above => "notebook.shell.run.above",
            Self::Below => "notebook.shell.run.below",
            Self::Selected => "notebook.shell.run.selected",
            Self::ValidateOnly => "notebook.shell.run.validate_only",
        }
    }

    /// Whether the scope needs an active cell to anchor on.
    pub fn needs_anchor(self) -> bool {
        matches!(
            self,
            Self::Cell | Self::CellWithUpstream | Self::Downstream | Self::Above | Self::Below
        )
    }

    fn request(self, anchor: Option<&str>, selected: &[String]) -> Option<RunRequest> {
        let anchor = || anchor.map(str::to_string);
        Some(match self {
            Self::Cell => RunRequest::Cell {
                id: anchor()?,
                include_stale_upstream: false,
            },
            Self::CellWithUpstream => RunRequest::Cell {
                id: anchor()?,
                include_stale_upstream: true,
            },
            Self::All => RunRequest::All,
            Self::Stale => RunRequest::Stale,
            Self::Downstream => RunRequest::Downstream(anchor()?),
            Self::Above => RunRequest::Above(anchor()?),
            Self::Below => RunRequest::Below(anchor()?),
            Self::Selected => RunRequest::Selected(selected.to_vec()),
            Self::ValidateOnly => RunRequest::ValidateOnly,
        })
    }
}

/// The core planner's answer, in the terms the shell runs it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlannedRun {
    /// Cells to execute, in run order.
    pub execute: Vec<String>,
    /// Cells whose current output is reused.
    pub reused: Vec<String>,
    /// Cells to check without running (Check Without Running).
    pub validate: Vec<String>,
    /// Cells the plan cannot run, with the reason.
    pub blocked: Vec<(String, DisplayMessage)>,
    /// The core plan itself, which the shell's scheduler runs.
    pub plan: RunPlan,
}

/// One outline row: a Markdown heading or a named cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineEntry {
    pub cell_id: String,
    /// Heading level (1–6); 0 for a named cell.
    pub level: u8,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub cell_id: String,
    /// Zero-based line within the cell.
    pub line: usize,
    pub snippet: String,
}

pub const MAX_SEARCH_HITS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageRow {
    pub cell_id: String,
    pub reads: Vec<String>,
    pub used_by: Vec<String>,
}

#[cfg(test)]
thread_local! {
    /// Times this thread rebuilt a notebook's fingerprints after an edit.
    pub(super) static FINGERPRINT_PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Times this thread rebuilt a notebook's dependency graph.
    pub(super) static GRAPH_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Which cells' source changed since a [`EditMark`], so the outline and
/// search can re-read only those cells after typing. Any other change to
/// the document starts a new generation, which makes every older mark
/// stale.
#[derive(Default)]
struct SourceEdits {
    generation: StdCell<u64>,
    version: StdCell<u64>,
    /// Edited cells in order; a run of edits to one cell is one entry.
    log: RefCell<Vec<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EditMark {
    generation: u64,
    version: u64,
    len: usize,
}

enum Since {
    Unchanged,
    Sources(Vec<String>),
    Everything,
}

impl SourceEdits {
    fn structure_changed(&self) {
        self.generation.set(self.generation.get() + 1);
        self.log.borrow_mut().clear();
    }

    fn source_changed(&self, id: &str) {
        self.version.set(self.version.get() + 1);
        let mut log = self.log.borrow_mut();
        if log.last().map(String::as_str) != Some(id) {
            log.push(id.to_string());
        }
    }

    fn mark(&self) -> EditMark {
        EditMark {
            generation: self.generation.get(),
            version: self.version.get(),
            len: self.log.borrow().len(),
        }
    }

    fn since(&self, mark: &EditMark) -> Since {
        if mark.generation != self.generation.get() {
            return Since::Everything;
        }
        if mark.version == self.version.get() {
            return Since::Unchanged;
        }
        // The entry last seen may have been edited again since.
        let mut ids: Vec<String> = self.log.borrow()[mark.len.saturating_sub(1)..].to_vec();
        ids.sort();
        ids.dedup();
        Since::Sources(ids)
    }
}

/// A cell's outline rows: its Markdown headings, or its name.
fn cell_outline(cell: &Cell, out: &mut Vec<OutlineEntry>) {
    match cell.kind {
        CellKind::Markdown => {
            let mut in_fence = false;
            for line in cell.source.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("```") {
                    in_fence = !in_fence;
                    continue;
                }
                if in_fence {
                    continue;
                }
                let level = trimmed.bytes().take_while(|b| *b == b'#').count();
                if (1..=6).contains(&level)
                    && trimmed[level..].starts_with(' ')
                    && !trimmed[level..].trim().is_empty()
                {
                    out.push(OutlineEntry {
                        cell_id: cell.id.clone(),
                        level: level as u8,
                        text: trimmed[level..].trim().to_string(),
                    });
                }
            }
        }
        _ => {
            if let Some(name) = &cell.name {
                out.push(OutlineEntry {
                    cell_id: cell.id.clone(),
                    level: 0,
                    text: name.clone(),
                });
            }
        }
    }
}

/// A cell's search hits (`true` marks a match on its name), at most
/// `lines` of them on source lines; true when that limit was reached.
fn cell_hits(cell: &Cell, needle: &str, out: &mut Vec<(bool, SearchHit)>, lines: usize) -> bool {
    if cell
        .name
        .as_deref()
        .is_some_and(|n| n.to_lowercase().contains(needle))
    {
        out.push((
            true,
            SearchHit {
                cell_id: cell.id.clone(),
                line: 0,
                snippet: cell.relation_name().to_string(),
            },
        ));
    }
    let mut found = 0;
    for (line_no, line) in cell.source.lines().enumerate() {
        if line.to_lowercase().contains(needle) {
            out.push((
                false,
                SearchHit {
                    cell_id: cell.id.clone(),
                    line: line_no,
                    snippet: line.trim().chars().take(120).collect(),
                },
            ));
            found += 1;
            if found >= lines {
                return true;
            }
        }
    }
    false
}

/// The hits for one search, by cell index, and the edits they reflect.
struct FoundText {
    needle: String,
    seen: EditMark,
    found: Vec<(usize, bool, SearchHit)>,
}

impl FoundText {
    fn hits(&self) -> Vec<SearchHit> {
        self.found.iter().map(|(_, _, h)| h.clone()).collect()
    }

    fn line_hits(&self) -> usize {
        self.found.iter().filter(|(_, name, _)| !name).count()
    }
}

pub struct ShellDoc {
    doc: NotebookDoc,
    db: DatabaseType,
    /// The tab's connection: what cells with no connection slot run on.
    tab: Option<ConnectionChoice>,
    choices: Vec<ConnectionChoice>,
    state: NotebookState,
    /// Parameter values that are not the saved defaults: personal values
    /// and sensitive values typed this session.
    supplied: BTreeMap<String, ParamValue>,
    clock: Rc<dyn Clock>,
    graph: RefCell<Option<Graph>>,
    /// Fingerprints, and the day they were computed for: date presets
    /// resolve against the clock, so a new day recomputes them.
    fingerprints: RefCell<Option<(chrono::NaiveDate, BTreeMap<String, CellFingerprint>)>>,
    /// The short form of the fingerprints last computed, for drawing: a
    /// frame drawn between an edit and its debounce shows these rather
    /// than refingerprinting the whole notebook.
    shown_fingerprints: RefCell<BTreeMap<String, String>>,
    /// The graph before the last edit, for drawing until it is rebuilt.
    shown_graph: RefCell<Option<Graph>>,
    /// Current values of scalar cells, bound into SQL that references them.
    scalar_values: BTreeMap<String, ScalarValue>,
    edits: SourceEdits,
    outline_cache: RefCell<Option<(EditMark, Rc<Vec<OutlineEntry>>)>>,
    search_cache: RefCell<Option<FoundText>>,
}

impl ShellDoc {
    pub fn new(doc: NotebookDoc) -> Self {
        Self {
            doc,
            db: DatabaseType::SqlServer,
            tab: None,
            choices: Vec::new(),
            state: NotebookState::default(),
            supplied: BTreeMap::new(),
            clock: Rc::new(SystemClock),
            graph: RefCell::new(None),
            fingerprints: RefCell::new(None),
            shown_fingerprints: RefCell::new(BTreeMap::new()),
            shown_graph: RefCell::new(None),
            scalar_values: BTreeMap::new(),
            edits: SourceEdits::default(),
            outline_cache: RefCell::new(None),
            search_cache: RefCell::new(None),
        }
    }

    /// The document, for saving and exports.
    pub fn model(&self) -> &NotebookDoc {
        &self.doc
    }

    #[cfg(test)]
    pub fn model_mut(&mut self) -> &mut NotebookDoc {
        self.invalidate();
        &mut self.doc
    }

    pub fn id(&self) -> &str {
        &self.doc.notebook_id
    }

    /// Give the notebook a new id (a copy of another notebook taking its
    /// own identity).
    pub fn set_id(&mut self, id: String) {
        self.doc.notebook_id = id;
        self.invalidate();
    }

    pub fn title(&self) -> Option<&str> {
        Some(self.doc.title.as_str()).filter(|t| !t.is_empty())
    }

    pub fn set_title(&mut self, title: &str) -> bool {
        let title = title.trim();
        if self.doc.title == title {
            return false;
        }
        self.doc.title = title.to_string();
        true
    }

    /// What the host knows about connections: the engine of the tab's
    /// connection, the tab's connection itself, and every local profile.
    pub fn set_connections(
        &mut self,
        db: Option<DatabaseType>,
        tab: Option<ConnectionChoice>,
        choices: Vec<ConnectionChoice>,
    ) {
        let db = db
            .or(tab.as_ref().map(|c| c.database_type))
            .unwrap_or(self.db);
        if db != self.db || tab != self.tab || choices != self.choices {
            self.db = db;
            self.tab = tab;
            self.choices = choices;
            self.invalidate();
        }
    }

    pub fn database_type(&self) -> DatabaseType {
        self.db
    }

    fn invalidate(&self) {
        self.edits.structure_changed();
        self.invalidate_derived();
    }

    /// Moves on every change to the document other than a cell's source,
    /// for caches that don't read the source (the in-app trust).
    pub(super) fn generation(&self) -> u64 {
        self.edits.generation.get()
    }

    /// Drop the graph and fingerprints (they are rebuilt when next read).
    fn invalidate_derived(&self) {
        if let Some(graph) = self.graph.borrow_mut().take() {
            *self.shown_graph.borrow_mut() = Some(graph);
        }
        *self.fingerprints.borrow_mut() = None;
    }

    fn choice_for_slot(&self, slot: &ConnectionRef) -> Option<&ConnectionChoice> {
        slot.profile_id
            .as_deref()
            .and_then(|id| self.choices.iter().find(|c| c.profile_id == id))
    }

    pub(super) fn dialects(&self) -> Dialects {
        let mut dialects = Dialects::single(self.db);
        for slot in &self.doc.connection_refs {
            if let Some(choice) = self.choice_for_slot(slot) {
                dialects
                    .by_connection_ref
                    .insert(slot.id.clone(), choice.database_type);
            }
        }
        dialects
    }

    pub(super) fn graph(&self) -> Ref<'_, Graph> {
        if self.graph.borrow().is_none() {
            #[cfg(test)]
            GRAPH_BUILDS.with(|n| n.set(n.get() + 1));
            *self.graph.borrow_mut() = Some(Graph::build(&self.doc, &self.dialects()));
        }
        Ref::map(self.graph.borrow(), |g| g.as_ref().expect("graph built"))
    }

    /// Every parameter's value (supplied, else its default) or error.
    pub fn resolved_params(&self) -> ResolvedParams {
        params::resolve_all(&self.doc.parameters, &self.supplied, self.clock.as_ref())
    }

    fn identities(&self) -> BTreeMap<String, ConnectionIdentity> {
        let identity = |choice: &ConnectionChoice| ConnectionIdentity {
            profile_id: choice.profile_id.clone(),
            database: choice.database.clone(),
            login_identity: choice.login_identity.clone(),
            engine: format!("{:?}", choice.database_type),
            policy_flags: choice.policy_flags.clone(),
            ..ConnectionIdentity::default()
        };
        let mut out = BTreeMap::new();
        if let Some(tab) = &self.tab {
            out.insert(DEFAULT_CONNECTION.to_string(), identity(tab));
        }
        for slot in &self.doc.connection_refs {
            if let Some(choice) = self.choice_for_slot(slot) {
                let mut id = identity(choice);
                id.database = slot.database.clone().or(id.database);
                out.insert(slot.id.clone(), id);
            }
        }
        out
    }

    fn fingerprints(&self) -> Ref<'_, BTreeMap<String, CellFingerprint>> {
        #[cfg(test)]
        FINGERPRINT_PASSES
            .with(|n| n.set(n.get() + usize::from(self.fingerprints.borrow().is_none())));
        let today = self.clock.today();
        let current = self
            .fingerprints
            .borrow()
            .as_ref()
            .is_some_and(|(day, _)| *day == today);
        if !current {
            let dialects = self.dialects();
            let identities = self.identities();
            let resolved = self.resolved_params();
            let graph = self.graph();
            let compute = |overrides: &BTreeMap<String, sqlly_services::notebook::Fingerprint>| {
                fingerprint_all(
                    &self.doc,
                    &graph,
                    &FingerprintInputs {
                        dialects: &dialects,
                        connections: &identities,
                        params: &resolved,
                        upstream_override: overrides,
                    },
                )
            };
            let mut fps = compute(&BTreeMap::new());
            // A scalar's value is bound into the SQL that references it as
            // a literal, so dependents see the value, not only the cell.
            if !self.scalar_values.is_empty() {
                let overrides: BTreeMap<String, sqlly_services::notebook::Fingerprint> = self
                    .scalar_values
                    .iter()
                    .filter_map(|(id, value)| {
                        let own = fps.get(id)?;
                        Some((
                            id.clone(),
                            sqlly_services::notebook::Fingerprint(
                                sqlly_services::notebook::run_record::sql_hash(&format!(
                                    "{}|{value:?}",
                                    own.value.0
                                )),
                            ),
                        ))
                    })
                    .collect();
                fps = compute(&overrides);
            }
            drop(graph);
            *self.shown_fingerprints.borrow_mut() = fps
                .iter()
                .map(|(id, f)| (id.clone(), f.value.short().to_string()))
                .collect();
            *self.fingerprints.borrow_mut() = Some((today, fps));
        }
        Ref::map(self.fingerprints.borrow(), |f| {
            &f.as_ref().expect("fingerprints built").1
        })
    }

    // ── Cells ─────────────────────────────────────────────────────────────

    pub fn len(&self) -> usize {
        self.doc.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.doc.cells.is_empty()
    }

    pub fn cell(&self, index: usize) -> Option<CellView<'_>> {
        self.doc.cells.get(index).map(|c| view_of(&self.doc, c))
    }

    pub fn cell_by_id(&self, id: &str) -> Option<CellView<'_>> {
        self.doc.cell(id).map(|c| view_of(&self.doc, c))
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.doc.cell_index(id)
    }

    pub fn cells(&self) -> impl Iterator<Item = CellView<'_>> {
        self.doc.cells.iter().map(|c| view_of(&self.doc, c))
    }

    /// Insert a new `kind` cell at `index` with the notebook's default
    /// policy (read-only unless the notebook says otherwise); returns its id.
    pub fn insert(&mut self, kind: &str, index: usize) -> String {
        let id = new_cell_id(&mut RandomIds, |id| self.doc.cell(id).is_some());
        let cell = Cell::new(
            id.clone(),
            CellKind::parse(kind),
            self.doc.settings.default_policy.clone(),
        );
        let index = index.min(self.doc.cells.len());
        self.doc.cells.insert(index, cell);
        self.invalidate();
        id
    }

    #[cfg(test)]
    pub fn remove(&mut self, index: usize) -> Option<RemovedCell> {
        let cell = self.doc.remove_cell(index)?;
        self.state.retain(&self.doc);
        self.invalidate();
        Some(RemovedCell { index, cell })
    }

    #[cfg(test)]
    pub fn restore(&mut self, removed: RemovedCell) -> usize {
        let index = removed.index.min(self.doc.cells.len());
        self.doc.cells.insert(index, removed.cell);
        self.invalidate();
        index
    }

    pub fn move_cell(&mut self, from: usize, to: usize) -> bool {
        let moved = self.doc.move_cell(from, to);
        if moved {
            self.invalidate();
        }
        moved
    }

    pub fn set_source(&mut self, id: &str, source: &str) -> bool {
        match self.doc.cell_mut(id) {
            Some(cell) if cell.source != source => {
                cell.source = source.to_string();
                self.edits.source_changed(id);
                self.invalidate_derived();
                true
            }
            _ => false,
        }
    }

    /// Switch a cell between SQL and Markdown. Other kinds are preserved as
    /// they are.
    pub fn set_kind(&mut self, id: &str, kind: &str) -> bool {
        let next = CellKind::parse(kind);
        let convertible = |k: &CellKind| matches!(k, CellKind::Sql | CellKind::Markdown);
        match self.doc.cell_mut(id) {
            Some(cell) if cell.kind != next && convertible(&cell.kind) && convertible(&next) => {
                cell.kind = next;
                self.invalidate();
                true
            }
            _ => false,
        }
    }

    /// Change the document in one step the helpers above do not cover
    /// (applying or undoing a group of AI proposals); caches and per-cell
    /// state follow.
    pub fn edit_model<R>(&mut self, edit: impl FnOnce(&mut NotebookDoc) -> R) -> R {
        let result = edit(&mut self.doc);
        self.state.retain(&self.doc);
        self.invalidate();
        result
    }

    /// [`Self::edit_model`], handing back the state of every cell the edit
    /// removed (so an undo can put it back).
    pub fn edit_model_parking<R>(
        &mut self,
        edit: impl FnOnce(&mut NotebookDoc) -> R,
    ) -> (R, Vec<(String, CellState)>) {
        let result = edit(&mut self.doc);
        let present: std::collections::HashSet<&str> =
            self.doc.cells.iter().map(|c| c.id.as_str()).collect();
        let gone: Vec<String> = self
            .state
            .cells
            .keys()
            .filter(|id| !present.contains(id.as_str()))
            .cloned()
            .collect();
        let parked = gone
            .into_iter()
            .filter_map(|id| self.state.cells.remove(&id).map(|state| (id, state)))
            .collect();
        self.invalidate();
        (result, parked)
    }

    /// Put back the state a removed cell had.
    pub fn put_state(&mut self, id: &str, state: CellState) {
        if self.doc.cell(id).is_some() {
            self.state.cells.insert(id.to_string(), state);
        }
    }

    /// The remote cells behind `cells` (themselves and what they read),
    /// in run order: what refreshing them queries.
    pub fn remote_cells_behind(&self, cells: &[&str]) -> Vec<String> {
        sqlly_services::notebook::cells::layout::remote_cells_behind(
            &self.doc,
            &self.graph(),
            cells,
        )
    }

    pub fn toggle_collapsed(&mut self, id: &str) -> bool {
        match self.doc.cell_mut(id) {
            Some(cell) => {
                cell.collapsed = !cell.collapsed;
                true
            }
            None => false,
        }
    }

    /// Rename a cell (empty clears the name); dependents' `{{old}}`
    /// references follow. Errors come back as display text.
    pub fn set_name(&mut self, id: &str, name: &str) -> Result<bool, String> {
        let name = name.trim();
        let next = (!name.is_empty()).then_some(name);
        if self.doc.cell(id).and_then(|c| c.name.as_deref()) == next {
            return Ok(false);
        }
        let dialects = self.dialects();
        references::rename_cell(&mut self.doc, id, next, &dialects)
            .map_err(|e| text(e.message()))?;
        self.invalidate();
        Ok(true)
    }

    pub fn set_read_only(&mut self, id: &str, read_only: bool) -> bool {
        match self.doc.cell_mut(id) {
            Some(cell) if cell.execution_policy.read_only != read_only => {
                cell.execution_policy.read_only = read_only;
                self.invalidate();
                true
            }
            _ => false,
        }
    }

    /// Whether the cell's SQL changes data, judged for its engine.
    pub fn writes(&self, id: &str) -> bool {
        let Some(cell) = self.doc.cell(id) else {
            return false;
        };
        let db = self.dialects().for_cell(&self.doc, cell);
        sqlly_services::notebook::migrate::writes_data(Some(db), &cell.source)
    }

    // ── Connections ───────────────────────────────────────────────────────

    /// The notebook's default connection slot: the first declared one.
    /// The scalar cells' current values, bound as literals into SQL.
    pub fn set_scalar_values(&mut self, values: BTreeMap<String, ScalarValue>) {
        if self.scalar_values != values {
            self.scalar_values = values;
            *self.fingerprints.borrow_mut() = None;
        }
    }

    pub fn notebook_connection(&self) -> Option<&ConnectionRef> {
        self.doc.connection_refs.first()
    }

    /// The slot a cell runs on: its own, else the notebook's.
    pub fn effective_connection(&self, id: &str) -> Option<&ConnectionRef> {
        let cell = self.doc.cell(id)?;
        self.doc
            .effective_connection_ref(cell)
            .and_then(|slot| self.doc.connection(slot))
    }

    fn slot_for(&mut self, choice: &ConnectionChoice) -> String {
        if let Some(slot) = self
            .doc
            .connection_refs
            .iter()
            .find(|s| s.profile_id.as_deref() == Some(choice.profile_id.as_str()))
        {
            return slot.id.clone();
        }
        let taken = |id: &str, refs: &[ConnectionRef]| refs.iter().any(|s| s.id == id);
        let mut n = self.doc.connection_refs.len() + 1;
        while taken(&format!("conn_{n}"), &self.doc.connection_refs) {
            n += 1;
        }
        let id = format!("conn_{n}");
        self.doc.connection_refs.push(slot_ref(&id, choice));
        id
    }

    /// Pin a cell to `choice`, or back to the notebook's connection. The
    /// first slot is every other cell's default, so a notebook that had none
    /// first records the tab's connection as its default.
    pub fn set_cell_connection(&mut self, id: &str, choice: Option<ConnectionChoice>) -> bool {
        if self.doc.cell(id).is_none() {
            return false;
        }
        let slot = match choice {
            Some(choice) => {
                if self.doc.connection_refs.is_empty()
                    && let Some(tab) = self.tab.clone()
                    && tab.profile_id != choice.profile_id
                {
                    self.slot_for(&tab);
                }
                Some(self.slot_for(&choice))
            }
            None => None,
        };
        match self.doc.cell_mut(id) {
            Some(cell) if cell.connection_ref != slot => {
                cell.connection_ref = slot;
                self.invalidate();
                true
            }
            _ => false,
        }
    }

    /// Whether "Use the tab's connection" can take effect: the first slot
    /// is every unpinned cell's default, so the notebook can only follow
    /// the tab when no cell is pinned to a slot other than the default.
    pub fn can_follow_tab(&self) -> bool {
        let first = self.doc.connection_refs.first().map(|s| s.id.as_str());
        self.doc
            .cells
            .iter()
            .filter_map(|c| c.connection_ref.as_deref())
            .all(|slot| Some(slot) == first)
    }

    /// Make `choice` the notebook's default connection (`None`: follow the
    /// tab again; cells pinned to the default slot follow it too, and slots
    /// no cell names any more go). Refused while a cell is pinned to another
    /// slot ([`Self::can_follow_tab`]).
    pub fn set_notebook_connection(&mut self, choice: Option<ConnectionChoice>) -> bool {
        match choice {
            Some(choice) => {
                let was_empty = self.doc.connection_refs.is_empty();
                let slot = self.slot_for(&choice);
                let at = self
                    .doc
                    .connection_refs
                    .iter()
                    .position(|s| s.id == slot)
                    .unwrap_or(0);
                if at == 0 && !was_empty {
                    return false;
                }
                let moved = self.doc.connection_refs.remove(at);
                self.doc.connection_refs.insert(0, moved);
            }
            None => {
                if self.doc.connection_refs.is_empty() || !self.can_follow_tab() {
                    return false;
                }
                for cell in &mut self.doc.cells {
                    cell.connection_ref = None;
                }
                self.doc.connection_refs.clear();
            }
        }
        self.invalidate();
        true
    }

    /// Slots this machine has no profile for.
    pub fn unresolved_connections(&self) -> Vec<ConnectionRef> {
        self.doc
            .connection_refs
            .iter()
            .filter(|s| self.choice_for_slot(s).is_none())
            .cloned()
            .collect()
    }

    /// Map slot `slot_id` to a local profile; returns how many cells it serves.
    pub fn remap_connection(&mut self, slot_id: &str, choice: &ConnectionChoice) -> usize {
        let Some(slot) = self
            .doc
            .connection_refs
            .iter_mut()
            .find(|s| s.id == slot_id)
        else {
            return 0;
        };
        slot.profile_id = Some(choice.profile_id.clone());
        slot.label = choice.label.clone();
        if slot.database.is_none() {
            slot.database = choice.database.clone();
        }
        self.invalidate();
        let first = self.doc.connection_refs.first().map(|s| s.id.as_str()) == Some(slot_id);
        self.doc
            .cells
            .iter()
            .filter(|c| {
                c.connection_ref.as_deref() == Some(slot_id)
                    || (first && c.connection_ref.is_none())
            })
            .count()
    }

    // ── Parameters ────────────────────────────────────────────────────────

    pub fn param_defs(&self) -> &[ParameterDef] {
        &self.doc.parameters
    }

    /// Each parameter's name and its saved default as text.
    pub fn params(&self) -> Vec<(String, String)> {
        self.doc
            .parameters
            .iter()
            .map(|p| {
                let value = match &p.default {
                    None | Some(serde_json::Value::Null) => String::new(),
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                };
                (p.name.clone(), value)
            })
            .collect()
    }

    /// Use the value `raw` parses to for parameter `name` (not saved).
    pub fn set_param(&mut self, name: &str, raw: &str) -> Result<bool, String> {
        let Some(def) = self.doc.parameters.iter().find(|p| p.name == name) else {
            return Ok(false);
        };
        let value = if raw.is_empty() {
            None
        } else {
            Some(params::parse_text(def, raw).map_err(|e| text(e.message()))?)
        };
        Ok(self.set_supplied(name, value))
    }

    pub fn supplied(&self) -> &BTreeMap<String, ParamValue> {
        &self.supplied
    }

    /// Use `value` for parameter `name` instead of its saved default, or go
    /// back to the default with `None`.
    pub fn set_supplied(&mut self, name: &str, value: Option<ParamValue>) -> bool {
        let changed = match value {
            Some(value) => self.supplied.insert(name.to_string(), value.clone()) != Some(value),
            None => self.supplied.remove(name).is_some(),
        };
        if changed {
            self.invalidate();
        }
        changed
    }

    pub fn set_clock(&mut self, clock: Rc<dyn Clock>) {
        self.clock = clock;
        self.invalidate();
    }

    /// Change parameter definitions, defaults or sets in the document.
    pub fn edit_params<R>(&mut self, edit: impl FnOnce(&mut NotebookDoc) -> R) -> R {
        let out = edit(&mut self.doc);
        self.invalidate();
        out
    }

    pub fn add_param_def(&mut self, def: ParameterDef) -> Result<(), String> {
        references::validate_parameter_name(&self.doc, None, &def.name)
            .map_err(|e| text(e.message()))?;
        self.doc.parameters.push(def);
        self.invalidate();
        Ok(())
    }

    /// Rename a parameter and every use of it (SQL, Markdown, parameter
    /// cells, other parameters' option queries).
    pub fn rename_param(&mut self, old: &str, new: &str) -> Result<bool, String> {
        if old == new {
            return Ok(false);
        }
        let dialects = self.dialects();
        param_cells::rename_parameter(&mut self.doc, old, new, &dialects)
            .map_err(|e| text(e.message()))?;
        if let Some(value) = self.supplied.remove(old) {
            self.supplied.insert(new.to_string(), value);
        }
        self.invalidate();
        Ok(true)
    }

    pub fn run_mode(&self) -> RunMode {
        self.doc.settings.run_mode
    }

    pub fn set_run_mode(&mut self, mode: RunMode) -> bool {
        let changed = self.doc.settings.run_mode != mode;
        self.doc.settings.run_mode = mode;
        changed
    }

    /// Markdown with `{{param.name}}` shown as current values.
    #[cfg(test)]
    pub fn interpolate_markdown(&self, text: &str) -> String {
        param_cells::interpolate_markdown(text, &self.doc.parameters, &self.resolved_params())
    }

    /// Which cells a change to `changed` re-runs at once and which wait for
    /// Apply.
    pub fn reactive_plan(&self, changed: &[String]) -> ReactivePlan {
        param_cells::reactive_plan(&self.doc, &self.graph(), changed)
    }

    pub fn remove_param(&mut self, name: &str) -> bool {
        let before = self.doc.parameters.len();
        self.doc.parameters.retain(|p| p.name != name);
        let removed = before != self.doc.parameters.len();
        if removed {
            self.supplied.remove(name);
            self.invalidate();
        }
        removed
    }

    // ── Graph ─────────────────────────────────────────────────────────────

    fn cells_of(edges: &[(Node, sqlly_services::notebook::graph::EdgeKind)]) -> Vec<String> {
        edges
            .iter()
            .filter_map(|(n, _)| n.as_cell().map(str::to_string))
            .collect()
    }

    /// Cells `id` reads from, directly.
    pub fn dependencies(&self, id: &str) -> Vec<String> {
        Self::cells_of(self.graph().upstream(&Node::cell(id)))
    }

    /// Cells that read from `id`, directly.
    pub fn dependents(&self, id: &str) -> Vec<String> {
        Self::cells_of(self.graph().downstream(&Node::cell(id)))
    }

    /// [`Self::dependencies`] and [`Self::dependents`] for drawing: from the
    /// graph as last built, so a frame drawn between an edit and its
    /// debounce does not rebuild it.
    pub fn shown_links(&self, id: &str) -> (Vec<String>, Vec<String>) {
        let node = Node::cell(id);
        if self.graph.borrow().is_none()
            && let Some(graph) = self.shown_graph.borrow().as_ref()
        {
            return (
                Self::cells_of(graph.upstream(&node)),
                Self::cells_of(graph.downstream(&node)),
            );
        }
        let graph = self.graph();
        (
            Self::cells_of(graph.upstream(&node)),
            Self::cells_of(graph.downstream(&node)),
        )
    }

    /// Every SQL cell with the cells it reads from and those that read from it.
    pub fn lineage(&self) -> Vec<LineageRow> {
        let graph = self.graph();
        self.doc
            .cells
            .iter()
            .filter(|c| c.kind == CellKind::Sql)
            .map(|c| {
                let node = Node::cell(&c.id);
                LineageRow {
                    cell_id: c.id.clone(),
                    reads: Self::cells_of(graph.upstream(&node)),
                    used_by: Self::cells_of(graph.downstream(&node)),
                }
            })
            .collect()
    }

    // ── Running ───────────────────────────────────────────────────────────

    /// Mark outputs stale (or current again) against the cells' current
    /// fingerprints. Returns the cells whose state changed.
    pub fn refresh_staleness(&mut self) -> Vec<String> {
        #[cfg(test)]
        super::perf_counters::bump(super::perf_counters::Pass::Staleness);
        let fps = self.fingerprints().clone();
        self.state.refresh_staleness(&fps)
    }

    /// What a run of `scope` does, from the core planner.
    pub fn plan(
        &mut self,
        scope: RunScope,
        anchor: Option<&str>,
        selected: &[String],
    ) -> PlannedRun {
        self.refresh_staleness();
        let Some(request) = scope.request(anchor, selected) else {
            return PlannedRun::default();
        };
        let resolved = self.resolved_params();
        let graph = self.graph();
        let fingerprints = self.fingerprints();
        let plan = sqlly_services::notebook::plan(
            &request,
            &PlanContext {
                doc: &self.doc,
                graph: &graph,
                fingerprints: &fingerprints,
                state: &self.state,
                params: &resolved,
                now_ms: chrono::Utc::now().timestamp_millis(),
            },
        );
        let mut out = PlannedRun::default();
        for step in &plan.steps {
            match step.action {
                StepAction::Execute => out.execute.push(step.cell_id.clone()),
                StepAction::Reuse(_) => out.reused.push(step.cell_id.clone()),
                StepAction::Validate => out.validate.push(step.cell_id.clone()),
                StepAction::Instant => {}
            }
        }
        out.blocked = plan
            .blocked
            .iter()
            .map(|b| (b.cell_id.clone(), b.reason.message()))
            .collect();
        out.plan = plan;
        out
    }

    /// The SQL a run of `id` sends: references expanded by the core, then
    /// parameters bound as literals for the cell's engine.
    pub fn sql_for_run(&self, id: &str) -> Result<String, String> {
        self.bound_run_sql(id, &self.doc.parameters)
            .map(|bound| bound.sql)
    }

    /// The steps of [`Self::sql_for_run`] with `defs` as the parameters to
    /// bind (any other `:name` is left as written).
    fn bound_run_sql(&self, id: &str, defs: &[ParameterDef]) -> Result<params::BoundSql, String> {
        let Some(cell) = self.doc.cell(id) else {
            return Err(crate::localization::text("notebook.shell.run.cell_missing"));
        };
        let dialects = self.dialects();
        let (db, sql) = if local_spec::targets_local(cell) {
            (DatabaseType::DuckDb, self.local_sql(id, &dialects)?)
        } else {
            (
                dialects.for_cell(&self.doc, cell),
                self.expanded_sql(id, &dialects)?,
            )
        };
        params::bind_sql(db, &sql, defs, &self.resolved_params()).map_err(|e| text(e.message()))
    }

    /// A connection cell's SQL with scalar values and `{{cell}}` references
    /// expanded by the core, parameters not yet bound.
    fn expanded_sql(&self, id: &str, dialects: &Dialects) -> Result<String, String> {
        let materialized = BTreeSet::new();
        let bound_doc =
            super::visual::scalar::bind_scalar_refs(&self.doc, dialects, &self.scalar_values);
        let doc = bound_doc.as_ref().unwrap_or(&self.doc);
        let expansion = expand_sql(
            doc,
            id,
            ExpandOptions {
                dialects,
                materialized: &materialized,
            },
        )
        .map_err(|e| text(e.message()))?;
        match expansion {
            Expansion::Sql(e) => Ok(e.sql),
            Expansion::NeedsLocalMaterialization { cells } => {
                let names: Vec<String> = cells
                    .iter()
                    .map(|c| {
                        self.doc
                            .cell(c)
                            .map(|c| c.relation_name().to_string())
                            .unwrap_or_else(|| c.clone())
                    })
                    .collect();
                Err(crate::localization::format(
                    "notebook.local.run.needs_local_target",
                    &[("cells", names.join(", "))],
                ))
            }
        }
    }

    /// What query history keeps for a run of `id` when it uses a sensitive
    /// parameter: the run's SQL with those parameters left as written, or a
    /// placeholder when that can't be built. `None`: nothing to hide.
    pub fn history_sql_for_run(&self, id: &str) -> Option<String> {
        let full = self.bound_run_sql(id, &self.doc.parameters).ok()?;
        let uses_redacted = full.used.iter().any(|name| {
            self.doc
                .parameter(name)
                .is_some_and(ParameterDef::is_redacted)
        });
        if !uses_redacted {
            return None;
        }
        let shown: Vec<ParameterDef> = self
            .doc
            .parameters
            .iter()
            .filter(|d| !d.is_redacted())
            .cloned()
            .collect();
        Some(
            self.bound_run_sql(id, &shown)
                .map(|bound| bound.sql)
                .unwrap_or_else(|_| params::UNRECORDED_SQL.to_string()),
        )
    }

    /// A local cell's SQL: each `{{x}}` replaced by the relation holding
    /// `x`'s output in local data, parameters not yet bound.
    fn local_sql(&self, id: &str, dialects: &Dialects) -> Result<String, String> {
        substitute_relations(&self.doc, id, dialects, &|cell: &Cell| {
            format!("\"{}\"", local_spec::local_relation(cell))
        })
        .map_err(|e| text(e.message()))
    }

    /// The relations a local cell reads, in dependency order. A cell that
    /// targets a connection has none.
    pub fn local_inputs(&self, id: &str) -> Result<Vec<LocalInput>, String> {
        let Some(cell) = self.doc.cell(id) else {
            return Err(crate::localization::text("notebook.shell.run.cell_missing"));
        };
        if !local_spec::targets_local(cell) {
            return Ok(Vec::new());
        }
        let dialects = self.dialects();
        let materialized = BTreeSet::new();
        let expansion = expand_sql(
            &self.doc,
            id,
            ExpandOptions {
                dialects: &dialects,
                materialized: &materialized,
            },
        )
        .map_err(|e| text(e.message()))?;
        let cells = match expansion {
            Expansion::Sql(_) => return Ok(Vec::new()),
            Expansion::NeedsLocalMaterialization { cells } => cells,
        };
        Ok(cells
            .into_iter()
            .filter_map(|cid| {
                let up = self.doc.cell(&cid)?;
                let source = match up.kind {
                    CellKind::Sql if local_spec::targets_local(up) => InputSource::LocalSql,
                    CellKind::Sql => InputSource::Remote,
                    CellKind::Attachment => InputSource::Attachment,
                    _ => InputSource::Other,
                };
                Some(LocalInput {
                    relation: local_spec::local_relation(up),
                    cell_id: cid,
                    source,
                })
            })
            .collect())
    }

    pub fn targets_local(&self, id: &str) -> bool {
        self.doc.cell(id).is_some_and(local_spec::targets_local)
    }

    pub fn set_targets_local(&mut self, id: &str, local: bool) -> bool {
        let changed = self
            .doc
            .cell_mut(id)
            .is_some_and(|c| local_spec::set_targets_local(c, local));
        if changed {
            self.invalidate();
        }
        changed
    }

    /// The relation holding `id`'s output in local data.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn local_relation(&self, id: &str) -> Option<String> {
        self.doc.cell(id).map(local_spec::local_relation)
    }

    pub fn materialized_as(&self, id: &str) -> Option<String> {
        self.doc
            .cell(id)
            .and_then(local_spec::materialized_as)
            .map(str::to_string)
    }

    pub fn suggested_relation(&self, id: &str) -> Option<String> {
        self.doc.cell(id).map(local_spec::suggested_relation)
    }

    pub fn set_materialized_as(&mut self, id: &str, relation: Option<&str>) -> bool {
        let changed = self
            .doc
            .cell_mut(id)
            .is_some_and(|c| local_spec::set_materialized_as(c, relation));
        if changed {
            self.invalidate();
        }
        changed
    }

    pub fn cache_settings(&self, id: &str) -> CacheSettings {
        self.doc
            .cell(id)
            .map(local_spec::cache_settings)
            .unwrap_or_default()
    }

    pub fn set_cache_settings(&mut self, id: &str, settings: CacheSettings) -> bool {
        let changed = self
            .doc
            .cell_mut(id)
            .is_some_and(|c| local_spec::set_cache_settings(c, settings));
        if changed {
            self.invalidate();
        }
        changed
    }

    pub fn set_cache_policy(
        &mut self,
        id: &str,
        policy: sqlly_services::notebook::CachePolicy,
    ) -> bool {
        match self.doc.cell_mut(id) {
            Some(cell) if cell.execution_policy.cache != policy => {
                cell.execution_policy.cache = policy;
                self.invalidate();
                true
            }
            _ => false,
        }
    }

    pub fn set_max_rows(&mut self, id: &str, max_rows: Option<u64>) -> bool {
        match self.doc.cell_mut(id) {
            Some(cell) if cell.execution_policy.max_rows != max_rows => {
                cell.execution_policy.max_rows = max_rows;
                self.invalidate();
                true
            }
            _ => false,
        }
    }

    pub fn auto_materialize(&self) -> bool {
        local_spec::auto_materialize(&self.doc)
    }

    pub fn set_auto_materialize(&mut self, on: bool) -> bool {
        local_spec::set_auto_materialize(&mut self.doc, on)
    }

    pub fn attachment(&self, id: &str) -> Option<AttachmentSpec> {
        self.doc
            .cell(id)
            .filter(|c| c.kind == CellKind::Attachment)
            .map(AttachmentSpec::of)
    }

    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn set_attachment(&mut self, id: &str, spec: &AttachmentSpec) -> bool {
        let changed = self
            .doc
            .cell_mut(id)
            .filter(|c| c.kind == CellKind::Attachment)
            .is_some_and(|c| spec.write(c));
        if changed {
            self.invalidate();
        }
        changed
    }

    pub fn session_group(&self, id: &str) -> Option<String> {
        self.doc.cell(id).and_then(|c| c.session_group.clone())
    }

    /// The connection identity `id`'s output was fetched as (the cache key's
    /// principal); `None` for cells that run in-process.
    pub fn connection_identity(&self, id: &str) -> Option<ConnectionIdentity> {
        let cell = self.doc.cell(id)?;
        if cell.exec_class() != sqlly_services::notebook::ExecClass::Remote {
            return None;
        }
        let slot = self
            .doc
            .effective_connection_ref(cell)
            .unwrap_or(DEFAULT_CONNECTION);
        self.identities().remove(slot)
    }

    /// The cell's current fingerprint, kept with a run so its result is tied
    /// to the inputs it ran with.
    pub fn cell_fingerprint(&self, id: &str) -> Option<CellFingerprint> {
        self.fingerprints().get(id).cloned()
    }

    /// Short form of the cell's fingerprint as last computed, for drawing:
    /// never refingerprints the notebook, except the first time.
    pub fn shown_fingerprint(&self, id: &str) -> Option<String> {
        if self.shown_fingerprints.borrow().is_empty() {
            return self.fingerprint_text(id);
        }
        self.shown_fingerprints.borrow().get(id).cloned()
    }

    /// Short form of the cell's current fingerprint, for display.
    pub fn fingerprint_text(&self, id: &str) -> Option<String> {
        self.fingerprints()
            .get(id)
            .map(|f| f.value.short().to_string())
    }

    pub fn cell_state(&self, id: &str) -> CellState {
        self.state.get(id)
    }

    pub fn status(&self, id: &str) -> CellStatus {
        self.state.get(id).status
    }

    /// Apply a state event (the core cascades blocking and staleness to
    /// dependents). An event that does not apply is ignored.
    pub fn apply(&mut self, id: &str, event: CellEvent) -> Vec<String> {
        let graph = self.graph().clone();
        self.state.apply(&graph, id, event).unwrap_or_default()
    }

    /// Record a finished run of `id` that started with `fingerprint`.
    pub fn complete(
        &mut self,
        id: &str,
        run_id: &str,
        fingerprint: CellFingerprint,
        row_count: u64,
    ) -> Vec<String> {
        self.complete_with(
            id,
            run_id,
            fingerprint,
            row_count,
            sqlly_services::notebook::Completeness::Complete,
        )
    }

    /// [`Self::complete`] for an output that is only `completeness` of the result.
    pub fn complete_with(
        &mut self,
        id: &str,
        run_id: &str,
        fingerprint: CellFingerprint,
        row_count: u64,
        completeness: sqlly_services::notebook::Completeness,
    ) -> Vec<String> {
        let output = OutputMeta {
            run_id: run_id.to_string(),
            fingerprint,
            completeness,
            row_count,
            finished_at_ms: chrono::Utc::now().timestamp_millis(),
            from_cache: false,
            freshness: sqlly_services::notebook::Freshness::Live,
        };
        let mut changed = self.apply(id, CellEvent::Completed { output });
        changed.extend(self.refresh_staleness());
        changed
    }

    /// Show a cached output as `id`'s result: current for its inputs, but
    /// labelled as cached (never as a fresh run).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn restore_cached(
        &mut self,
        id: &str,
        run_id: &str,
        fingerprint: CellFingerprint,
        row_count: u64,
        completeness: sqlly_services::notebook::Completeness,
    ) -> Vec<String> {
        let output = OutputMeta {
            run_id: run_id.to_string(),
            fingerprint,
            completeness,
            row_count,
            finished_at_ms: chrono::Utc::now().timestamp_millis(),
            from_cache: true,
            freshness: sqlly_services::notebook::Freshness::TtlCached,
        };
        self.apply(id, CellEvent::Completed { output })
    }

    // ── Outline and search ────────────────────────────────────────────────

    /// Markdown headings and named cells, in document order.
    #[cfg(test)]
    pub fn outline(&self) -> Vec<OutlineEntry> {
        self.outline_shared().as_ref().clone()
    }

    /// [`Self::outline`], shared and kept between frames: after text is
    /// typed, only the edited cells are read again, and the outline is
    /// rebuilt only when their headings changed.
    pub fn outline_shared(&self) -> Rc<Vec<OutlineEntry>> {
        let mut cache = self.outline_cache.borrow_mut();
        if let Some((seen, entries)) = cache.as_mut() {
            let same = |id: &String| {
                self.doc.cell(id).is_some_and(|cell| {
                    let mut now = Vec::new();
                    cell_outline(cell, &mut now);
                    now.iter().eq(entries.iter().filter(|e| e.cell_id == *id))
                })
            };
            match self.edits.since(seen) {
                Since::Unchanged => return entries.clone(),
                Since::Sources(ids) if ids.iter().all(same) => {
                    *seen = self.edits.mark();
                    return entries.clone();
                }
                _ => {}
            }
        }
        #[cfg(test)]
        super::perf_counters::bump(super::perf_counters::Pass::Outline);
        let mut out = Vec::new();
        for cell in &self.doc.cells {
            cell_outline(cell, &mut out);
        }
        let out = Rc::new(out);
        *cache = Some((self.edits.mark(), out.clone()));
        out
    }

    /// Case-insensitive search across cell sources and names. Kept per
    /// query: after text is typed, only the edited cells are searched again.
    pub fn search(&self, query: &str) -> Vec<SearchHit> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut cache = self.search_cache.borrow_mut();
        if let Some(found) = cache.as_mut().filter(|f| f.needle == needle) {
            match self.edits.since(&found.seen) {
                Since::Unchanged => return found.hits(),
                Since::Sources(ids) if found.line_hits() < MAX_SEARCH_HITS => {
                    for id in &ids {
                        found.found.retain(|(_, _, hit)| hit.cell_id != *id);
                        if let Some(index) = self.doc.cell_index(id) {
                            let mut hits = Vec::new();
                            cell_hits(&self.doc.cells[index], &needle, &mut hits, usize::MAX);
                            let at = found.found.partition_point(|(i, _, _)| *i < index);
                            found
                                .found
                                .splice(at..at, hits.into_iter().map(|(name, h)| (index, name, h)));
                        }
                    }
                    if found.line_hits() < MAX_SEARCH_HITS {
                        found.seen = self.edits.mark();
                        return found.hits();
                    }
                }
                _ => {}
            }
        }
        #[cfg(test)]
        super::perf_counters::bump(super::perf_counters::Pass::Search);
        let mut found = Vec::new();
        let mut lines = 0;
        for (index, cell) in self.doc.cells.iter().enumerate() {
            let mut hits = Vec::new();
            let full = cell_hits(cell, &needle, &mut hits, MAX_SEARCH_HITS - lines);
            lines += hits.iter().filter(|(name, _)| !name).count();
            found.extend(hits.into_iter().map(|(name, h)| (index, name, h)));
            if full {
                break;
            }
        }
        let found = FoundText {
            needle,
            seen: self.edits.mark(),
            found,
        };
        let hits = found.hits();
        *cache = Some(found);
        hits
    }
}

/// What the analysis cells and run history read and change.
impl ShellDoc {
    /// A cell's kind-specific configuration (`Value::Null` when unset).
    pub fn spec(&self, id: &str) -> serde_json::Value {
        self.doc
            .cell(id)
            .map(|c| c.spec.clone())
            .unwrap_or(serde_json::Value::Null)
    }

    pub fn set_spec(&mut self, id: &str, spec: serde_json::Value) -> bool {
        match self.doc.cell_mut(id) {
            Some(cell) if cell.spec != spec => {
                cell.spec = spec;
                self.invalidate();
                true
            }
            _ => false,
        }
    }

    /// The parameters' values as a run resolves them.
    pub fn resolved(&self) -> ResolvedParams {
        self.resolved_params()
    }

    /// Names of the parameters `id` uses.
    pub fn params_of(&self, id: &str) -> Vec<String> {
        self.graph()
            .params_of(id)
            .into_iter()
            .map(|p| p.to_string())
            .collect()
    }

    /// The engine `id` runs on.
    pub fn database_type_for(&self, id: &str) -> DatabaseType {
        match self.doc.cell(id) {
            Some(cell) => self.dialects().for_cell(&self.doc, cell),
            None => self.db,
        }
    }

    /// Who `id`'s queries run as, for cache scopes and run records.
    pub fn identity_for_cell(&self, id: &str) -> Option<ConnectionIdentity> {
        let slot = self
            .effective_connection(id)
            .map(|s| s.id.clone())
            .unwrap_or_else(|| DEFAULT_CONNECTION.to_string());
        self.identities().remove(&slot)
    }

    /// [`Self::sql_for_run`] with parameter `name` set to `raw` (sweeps).
    pub fn sql_for_run_with(&self, id: &str, name: &str, raw: &str) -> Result<String, String> {
        let mut copy = ShellDoc {
            doc: self.doc.clone(),
            db: self.db,
            tab: self.tab.clone(),
            choices: self.choices.clone(),
            state: NotebookState::default(),
            supplied: self.supplied.clone(),
            clock: self.clock.clone(),
            graph: RefCell::new(None),
            fingerprints: RefCell::new(None),
            shown_fingerprints: RefCell::new(BTreeMap::new()),
            shown_graph: RefCell::new(None),
            scalar_values: self.scalar_values.clone(),
            edits: SourceEdits::default(),
            outline_cache: RefCell::new(None),
            search_cache: RefCell::new(None),
        };
        copy.set_param(name, raw)?;
        copy.sql_for_run(id)
    }
}

fn slot_ref(id: &str, choice: &ConnectionChoice) -> ConnectionRef {
    choice.to_ref(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::ConnectionEnvironment;
    use sqlly_services::notebook::SeededIds;

    fn doc(sources: &[(&str, &str)]) -> ShellDoc {
        let mut nb = NotebookDoc::empty("nb_test", "Test");
        for (i, (kind, source)) in sources.iter().enumerate() {
            let at = nb.add_cell(CellKind::parse(kind), None, &mut SeededIds(i as u64 + 1));
            nb.cells[at].id = format!("c{}", i + 1);
            nb.cells[at].source = source.to_string();
        }
        let mut d = ShellDoc::new(nb);
        d.set_connections(
            Some(DatabaseType::SqlServer),
            Some(choice("tab", "Tab")),
            vec![choice("tab", "Tab"), choice("other", "Other")],
        );
        d
    }

    fn choice(id: &str, label: &str) -> ConnectionChoice {
        ConnectionChoice {
            profile_id: id.into(),
            label: label.into(),
            environment: ConnectionEnvironment::Dev,
            database_type: DatabaseType::SqlServer,
            database: None,
            ..ConnectionChoice::default()
        }
    }

    #[test]
    fn using_the_tab_connection_drops_the_default_even_when_cells_name_it() {
        let mut d = doc(&[
            ("sql", "SELECT 1"),
            ("sql", "SELECT 2"),
            ("sql", "SELECT 3"),
        ]);
        assert!(d.set_notebook_connection(Some(choice("other", "Other"))));
        let slot = d.notebook_connection().unwrap().id.clone();
        d.model_mut().cells[1].connection_ref = Some(slot);
        assert!(d.can_follow_tab());
        assert!(d.set_notebook_connection(None));
        assert!(d.model().connection_refs.is_empty());
        for id in ["c1", "c2", "c3"] {
            assert!(d.effective_connection(id).is_none(), "{id} follows the tab");
            assert_eq!(d.model().cell(id).unwrap().connection_ref, None);
        }
        assert!(!d.set_notebook_connection(None), "nothing left to drop");
    }

    #[test]
    fn a_cell_pinned_elsewhere_keeps_the_saved_default() {
        let mut d = doc(&[("sql", "SELECT 1"), ("sql", "SELECT 2")]);
        assert!(d.set_cell_connection("c2", Some(choice("other", "Other"))));
        assert_eq!(
            d.model().connection_refs.len(),
            2,
            "the tab became the default"
        );
        assert!(!d.can_follow_tab());
        assert!(!d.set_notebook_connection(None));
        assert_eq!(
            d.effective_connection("c1").unwrap().profile_id.as_deref(),
            Some("tab"),
            "unpinned cells never move to the other cell's connection"
        );
    }

    #[test]
    fn a_login_or_safety_change_is_another_principal() {
        let mut d = doc(&[("sql", "SELECT 1 AS x")]);
        let before = d.identity_for_cell("c1").unwrap();
        let fp = d.cell_fingerprint("c1").unwrap();
        for change in [
            |c: &mut ConnectionChoice| c.login_identity = "other-login".into(),
            |c: &mut ConnectionChoice| {
                c.policy_flags.insert("read_only".into());
            },
        ] {
            let mut tab = choice("tab", "Tab");
            change(&mut tab);
            d.set_connections(
                Some(DatabaseType::SqlServer),
                Some(tab.clone()),
                vec![tab, choice("other", "Other")],
            );
            assert_ne!(d.identity_for_cell("c1").unwrap(), before);
            assert_ne!(
                d.cell_fingerprint("c1").unwrap().value,
                fp.value,
                "outputs fetched as the old principal go stale"
            );
        }
    }

    #[test]
    fn history_never_records_a_sensitive_value_for_local_or_scalar_bound_cells() {
        let mut d = doc(&[
            ("sql", "SELECT 1 AS x"),
            ("scalar", ""),
            ("sql", "SELECT * FROM t WHERE n = {{c2}} AND pin = :pin"),
            ("sql", "SELECT * FROM {{c1}} WHERE pin = :pin"),
            ("sql", "SELECT * FROM t WHERE pin = :pin"),
        ]);
        local_spec::set_targets_local(d.doc.cell_mut("c4").unwrap(), true);
        let mut pin = ParameterDef::new("pin", params::ParamType::String);
        pin.sensitive = true;
        d.doc.parameters.push(pin);
        d.set_param("pin", "8675309").unwrap();
        d.set_scalar_values([("c2".to_string(), ScalarValue::Number(7.0))].into());
        for id in ["c3", "c4", "c5"] {
            let run = d.sql_for_run(id).unwrap();
            assert!(run.contains("8675309"), "{id}: the database gets it: {run}");
            let history = d.history_sql_for_run(id).expect("a redacted history copy");
            assert!(!history.contains("8675309"), "{id}: {history}");
        }
        let scalar = d.history_sql_for_run("c3").unwrap();
        assert!(scalar.contains(":pin") && scalar.contains('7'), "{scalar}");
        let local = d.history_sql_for_run("c4").unwrap();
        assert!(
            local.contains(":pin") && local.contains("\"c1\""),
            "{local}"
        );
        assert!(d.history_sql_for_run("c1").is_none());
    }

    #[test]
    fn plans_every_scope_through_the_core_planner() {
        let mut d = doc(&[
            ("sql", "SELECT 1 AS x"),
            ("markdown", "# Notes"),
            ("sql", "SELECT x FROM {{c1}}"),
            ("sql", "SELECT x FROM {{c3}}"),
            ("sql", "SELECT 9 AS y"),
        ]);
        let run = |d: &mut ShellDoc, scope, anchor: Option<&str>| {
            d.plan(scope, anchor, &["c5".into(), "c1".into()]).execute
        };
        assert_eq!(run(&mut d, RunScope::Cell, Some("c3")), ["c3"]);
        assert_eq!(
            run(&mut d, RunScope::CellWithUpstream, Some("c4")),
            ["c1", "c3", "c4"]
        );
        assert_eq!(run(&mut d, RunScope::All, None), ["c1", "c3", "c4", "c5"]);
        assert_eq!(
            run(&mut d, RunScope::Downstream, Some("c1")),
            ["c1", "c3", "c4"]
        );
        assert_eq!(run(&mut d, RunScope::Above, Some("c4")), ["c1", "c3"]);
        assert_eq!(run(&mut d, RunScope::Below, Some("c4")), ["c4", "c5"]);
        assert_eq!(run(&mut d, RunScope::Selected, None), ["c1", "c5"]);
        assert!(run(&mut d, RunScope::Cell, None).is_empty());
        assert!(
            !d.plan(RunScope::ValidateOnly, None, &[])
                .validate
                .is_empty()
        );
    }

    struct DayClock(std::cell::Cell<chrono::NaiveDateTime>);

    impl Clock for DayClock {
        fn now(&self) -> chrono::NaiveDateTime {
            self.0.get()
        }
    }

    #[test]
    fn date_presets_and_scalar_values_move_fingerprints() {
        let mut d = doc(&[
            ("sql", "SELECT * FROM t WHERE d = :day"),
            ("scalar", ""),
            ("sql", "SELECT {{c2}} AS target"),
        ]);
        let start = chrono::NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(23, 59, 0)
            .unwrap();
        let clock = Rc::new(DayClock(std::cell::Cell::new(start)));
        d.set_clock(clock.clone());
        let mut day = ParameterDef::new("day", sqlly_services::notebook::ParamType::Date);
        day.default = Some(serde_json::json!({"preset": "today"}));
        d.add_param_def(day).unwrap();
        let fp = d.cell_fingerprint("c1").unwrap();
        d.apply("c1", CellEvent::Started { run_id: "r".into() });
        d.complete("c1", "r", fp, 1);
        assert!(d.cell_state("c1").is_presentable_as_current());

        clock.0.set(start + chrono::Duration::minutes(2));
        d.refresh_staleness();
        assert!(
            !d.cell_state("c1").is_presentable_as_current(),
            "Today moved past midnight"
        );

        let before = d.cell_fingerprint("c3").unwrap();
        d.set_scalar_values(BTreeMap::from([(
            "c2".to_string(),
            ScalarValue::Number(5.0),
        )]));
        let five = d.cell_fingerprint("c3").unwrap();
        assert_ne!(before.value, five.value, "the bound literal is part of it");
        d.set_scalar_values(BTreeMap::from([(
            "c2".to_string(),
            ScalarValue::Number(6.0),
        )]));
        assert_ne!(five.value, d.cell_fingerprint("c3").unwrap().value);
        assert_eq!(
            d.cell_fingerprint("c2").unwrap().value,
            before_scalar(&d),
            "the scalar's own fingerprint is its definition"
        );
    }

    fn before_scalar(d: &ShellDoc) -> sqlly_services::notebook::Fingerprint {
        let mut plain = ShellDoc::new(d.model().clone());
        plain.set_connections(
            Some(DatabaseType::SqlServer),
            Some(choice("tab", "Tab")),
            vec![choice("tab", "Tab"), choice("other", "Other")],
        );
        plain.cell_fingerprint("c2").unwrap().value
    }

    #[test]
    fn completed_runs_go_stale_when_their_inputs_change() {
        let mut d = doc(&[("sql", "SELECT 1 AS x"), ("sql", "SELECT x FROM {{c1}}")]);
        for id in ["c1", "c2"] {
            let fp = d.cell_fingerprint(id).unwrap();
            let run_id = format!("r_{id}");
            d.apply(
                id,
                CellEvent::Started {
                    run_id: run_id.clone(),
                },
            );
            d.complete(id, &run_id, fp, 1);
            assert!(d.cell_state(id).is_presentable_as_current());
        }
        assert!(d.plan(RunScope::Stale, None, &[]).execute.is_empty());
        d.set_source("c1", "SELECT 2 AS x");
        d.refresh_staleness();
        assert!(!d.cell_state("c1").is_presentable_as_current());
        assert!(
            !d.cell_state("c2").is_presentable_as_current(),
            "the dependent is stale too"
        );
        assert_eq!(d.plan(RunScope::Stale, None, &[]).execute, ["c1", "c2"]);
        d.set_source("c1", "SELECT 1 AS x");
        d.refresh_staleness();
        assert!(
            d.cell_state("c2").is_presentable_as_current(),
            "reverting restores it"
        );
    }

    #[test]
    fn a_failure_blocks_queued_dependents() {
        let mut d = doc(&[
            ("sql", "SELECT 1 AS x"),
            ("sql", "SELECT x FROM {{c1}}"),
            ("sql", "SELECT 3"),
        ]);
        for id in ["c1", "c2", "c3"] {
            d.apply(id, CellEvent::Queued);
        }
        d.apply("c1", CellEvent::Started { run_id: "r".into() });
        d.apply(
            "c1",
            CellEvent::Failed {
                error: DisplayMessage::raw("boom"),
            },
        );
        assert_eq!(d.status("c2"), CellStatus::Blocked);
        assert_eq!(d.status("c3"), CellStatus::Queued);
    }

    #[test]
    fn expansion_and_parameters_go_through_the_core() {
        let mut d = doc(&[
            ("sql", "SELECT 1 AS x WHERE 1 = @min_id"),
            ("sql", "SELECT x FROM {{c1}}"),
        ]);
        let def = ParameterDef::new("min_id", sqlly_services::notebook::ParamType::Integer);
        assert_eq!(d.add_param_def(def.clone()), Ok(()));
        assert!(d.add_param_def(def).is_err());
        assert!(d.set_supplied("min_id", Some(ParamValue::Integer(5))));
        assert!(!d.set_supplied("min_id", Some(ParamValue::Integer(5))));
        let sql = d.sql_for_run("c2").unwrap();
        assert!(sql.contains("1 = 5"), "{sql}");
        assert!(sql.contains("c1"), "{sql}");
        assert!(d.sql_for_run("missing").is_err());
    }

    #[test]
    fn cells_on_another_connection_need_local_materialization() {
        let mut d = doc(&[("sql", "SELECT 1 AS x"), ("sql", "SELECT x FROM {{c1}}")]);
        d.set_cell_connection("c1", Some(choice("other", "Other")));
        let err = d.sql_for_run("c2").unwrap_err();
        assert!(err.contains("c1"), "{err}");
        assert_eq!(
            d.notebook_connection().unwrap().profile_id.as_deref(),
            Some("tab")
        );
        assert_eq!(
            d.effective_connection("c2").unwrap().profile_id.as_deref(),
            Some("tab"),
            "the other cells keep the tab's connection"
        );
    }

    #[test]
    fn renames_rewrite_dependents_and_report_core_errors() {
        let mut d = doc(&[("sql", "SELECT 1 AS x"), ("sql", "SELECT x FROM {{c1}}")]);
        assert_eq!(d.set_name("c1", "base"), Ok(true));
        assert_eq!(d.dependencies("c2"), ["c1"]);
        assert_eq!(d.dependents("c1"), ["c2"]);
        assert_eq!(d.lineage()[0].used_by, ["c2"]);
        assert!(
            d.set_name("c2", "select").is_err(),
            "reserved words are refused"
        );
        assert!(d.set_name("c2", "base").is_err());
    }

    #[test]
    fn unresolved_slots_remap_to_local_profiles() {
        let mut d = doc(&[("sql", "SELECT 1")]);
        d.model_mut().connection_refs.push(ConnectionRef {
            id: "wh".into(),
            profile_id: Some("theirs".into()),
            label: "Warehouse".into(),
            database: Some("Sales".into()),
            extra: Default::default(),
        });
        assert_eq!(d.unresolved_connections().len(), 1);
        assert_eq!(d.remap_connection("wh", &choice("other", "Other")), 1);
        assert!(d.unresolved_connections().is_empty());
        let slot = d.effective_connection("c1").unwrap();
        assert_eq!(slot.profile_id.as_deref(), Some("other"));
        assert_eq!(slot.database.as_deref(), Some("Sales"));
    }

    #[test]
    fn outline_search_delete_and_kinds() {
        let mut d = doc(&[
            (
                "markdown",
                "# Revenue\n```\n# not a heading\n```\n## By region",
            ),
            ("sql", "SELECT region FROM sales"),
            ("chart", ""),
        ]);
        d.set_name("c2", "regional").unwrap();
        let texts: Vec<(u8, String)> = d.outline().into_iter().map(|e| (e.level, e.text)).collect();
        assert_eq!(
            texts,
            [
                (1, "Revenue".into()),
                (2, "By region".into()),
                (0, "regional".into())
            ]
        );
        assert!(d.search("REGION").iter().any(|h| h.cell_id == "c2"));
        assert!(!d.set_kind("c3", KIND_SQL));
        assert!(d.set_kind("c2", KIND_MARKDOWN));
        let id = d.insert(KIND_SQL, 0);
        assert!(d.cell(0).unwrap().read_only(), "new cells start read-only");
        let removed = d.remove(0).unwrap();
        assert_eq!(removed.id(), id);
        assert_eq!(d.restore(removed), 0);
    }

    fn outline_rebuilds() -> usize {
        crate::notebook::perf_counters::get(crate::notebook::perf_counters::Pass::Outline)
    }

    fn texts(d: &ShellDoc) -> Vec<String> {
        d.outline_shared().iter().map(|e| e.text.clone()).collect()
    }

    #[test]
    fn typing_rereads_only_the_edited_cell_for_the_outline() {
        let mut d = doc(&[
            ("markdown", "# One"),
            ("sql", "SELECT 1"),
            ("markdown", "## Two"),
        ]);
        d.set_name("c2", "q").unwrap();
        assert_eq!(texts(&d), ["One", "q", "Two"]);
        let built = outline_rebuilds();
        d.set_source("c2", "SELECT 2");
        d.set_source("c3", "## Two\n\nMore notes");
        assert_eq!(texts(&d), ["One", "q", "Two"]);
        assert_eq!(outline_rebuilds(), built, "no heading changed");
        d.set_source("c3", "## Two\n\n### Three");
        assert_eq!(texts(&d), ["One", "q", "Two", "Three"]);
        d.set_source("c3", "## Two\n\n### Three!");
        assert_eq!(
            texts(&d),
            ["One", "q", "Two", "Three!"],
            "the same cell again"
        );
        d.set_name("c2", "renamed").unwrap();
        assert_eq!(texts(&d), ["One", "renamed", "Two", "Three!"]);
        assert_eq!(outline_rebuilds(), built + 3);
    }

    #[test]
    fn search_after_typing_matches_a_fresh_search() {
        let mut d = doc(&[
            ("sql", "SELECT a\nFROM orders"),
            ("markdown", "Orders by month"),
            ("sql", "SELECT b FROM lines"),
        ]);
        let ids = |hits: Vec<SearchHit>| -> Vec<(String, usize)> {
            hits.into_iter().map(|h| (h.cell_id, h.line)).collect()
        };
        assert_eq!(
            ids(d.search("orders")),
            [("c1".to_string(), 1), ("c2".to_string(), 0)]
        );
        d.set_source("c3", "SELECT b FROM orders");
        d.set_source("c1", "SELECT a");
        let typed = ids(d.search("orders"));
        assert_eq!(typed, [("c2".to_string(), 0), ("c3".to_string(), 0)]);
        let fresh = doc(&[
            ("sql", "SELECT a"),
            ("markdown", "Orders by month"),
            ("sql", "SELECT b FROM orders"),
        ]);
        assert_eq!(typed, ids(fresh.search("orders")));
        d.set_name("c1", "orders_q").unwrap();
        assert_eq!(ids(d.search("orders"))[0], ("c1".to_string(), 0));
    }
}
