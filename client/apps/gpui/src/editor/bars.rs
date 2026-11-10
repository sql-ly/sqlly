use super::*;

/// The editor's Go to Line bar. Created lazily on first open (its
/// `InputState` needs a `&mut Window`) and kept for the editor's lifetime.
pub(super) struct GoToLineBarState {
    pub(super) input: Entity<InputState>,
    pub(super) visible: bool,
    pub(super) _subscription: gpui::Subscription,
}

/// The editor's find/replace bar. Created lazily on first open (its
/// `InputState`s need a `&mut Window`) and kept for the editor's lifetime, so
/// the query, options, and replacement text survive close/reopen.
pub(super) struct FindBarState {
    pub(super) find: Entity<InputState>,
    pub(super) replace: Entity<InputState>,
    /// Bar visibility; hiding keeps the state, so highlights stop painting
    /// but the query is still there on the next open.
    pub(super) visible: bool,
    /// Whether the replace row is shown (the Replace action) or the bar is
    /// find-only (the Find action).
    pub(super) show_replace: bool,
    pub(super) case_sensitive: bool,
    pub(super) whole_word: bool,
    pub(super) regex: bool,
    /// Mirror of the find input's text — the canonical business value.
    pub(super) query: String,
    /// Mirror of the replace input's text.
    pub(super) replacement: String,
    /// Reused across document rescans until the query or an option changes.
    pub(super) compiled_query: Option<find_replace::CompiledFindQuery>,
    /// Invalid regex feedback shown directly under the search row.
    pub(super) compile_error: Option<String>,
    /// Every match of `query`, ascending byte ranges, capped at
    /// [`find_replace::MAX_MATCHES`]. `Rc` so the per-frame paint path shares
    /// the allocation instead of cloning up to 10k ranges.
    pub(super) matches: std::rc::Rc<Vec<Range<usize>>>,
    /// True when the scan stopped at the cap (the count label shows "+").
    pub(super) truncated: bool,
    /// Index into `matches` of the current match — the navigation target,
    /// painted stronger than the rest.
    pub(super) current: Option<usize>,
    /// Guards debounced rescans: only the latest scheduled rescan runs.
    pub(super) generation: u64,
    /// Keep the input subscriptions alive for the editor's lifetime.
    pub(super) _subscriptions: [Subscription; 2],
}
