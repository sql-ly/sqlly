use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
    RequireConfirmation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CommentMode {
    Toggle,
    Add,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSource {
    /// Produced by Validate / directive lint; replaced on the next validation.
    Validation,
    /// Produced by a failed run; dropped as soon as the buffer changes.
    Execution,
    /// Editor-owned live lint (currently the IDENTITY_INSERT check),
    /// recomputed with the debounced insert-hint pass; validation and
    /// execution pushes must leave these entries alone.
    Lint,
}

/// A one-click fix a diagnostic's hover popup offers as a "Fix this" link.
/// The fix is recomputed from the buffer at click time, so a diagnostic
/// painted a few keystrokes ago never applies stale text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticFix {
    /// Add the query's ungrouped select-list columns to its GROUP BY
    /// (writing the clause when there is none). `label` is the link text.
    AddToGroupBy { label: String },
}

impl DiagnosticFix {
    pub fn label(&self) -> &str {
        match self {
            DiagnosticFix::AddToGroupBy { label } => label,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ValidationDiagnostic {
    pub severity: DiagnosticSeverity,
    pub source: DiagnosticSource,
    pub message: String,
    pub line: usize,
    #[allow(dead_code)]
    pub identifier: Option<String>,
    pub start_byte: usize,
    pub end_byte: usize,
    /// Longer explanation shown under the message in the hover popup.
    pub explainer: Option<String>,
    /// A fix the hover popup offers as a clickable link.
    pub fix: Option<DiagnosticFix>,
}

/// Cached byte-offset index of line starts, rebuilt lazily when
/// `content_revision` changes. Converts O(n) line scans to O(1) or
/// O(log lines) lookups.
/// The `(content_revision, index)` cache slot, `Rc`-shared by every view of
/// one document so the index is rebuilt once per edit, not once per view.
pub(super) type SharedLineIndexCache =
    std::rc::Rc<std::cell::RefCell<Option<(u64, std::rc::Rc<LineIndex>)>>>;

/// The `(content_revision, dialect, states)` cache slot for the per-line
/// block-comment depths, `Rc`-shared by every view of one document. Keyed by
/// dialect as well as revision because switching the connection's engine
/// changes which comment syntax the same bytes parse under.
pub(super) type SharedCommentStateCache = std::rc::Rc<
    std::cell::RefCell<
        Option<(
            u64,
            sqlly_lang::sqlmodel::Dialect,
            std::rc::Rc<Vec<crate::sql_syntax::CommentState>>,
        )>,
    >,
>;

/// Cache slot for the widest-display-line scan:
/// `(content_revision, tab_width, insert_hints_version, cols)`.
pub(super) type MaxDisplayColsKey = (u64, usize, u64, usize);

pub(super) struct LineIndex {
    pub(super) starts: Vec<usize>,
    /// UTF-16 code-unit offset of each line start, parallel to `starts`.
    /// Lets byte↔UTF-16 conversion (the IME hot path) walk one line instead
    /// of the whole document.
    pub(super) utf16_starts: Vec<usize>,
}

impl LineIndex {
    pub(super) fn new(content: &str) -> Self {
        let mut starts = Vec::with_capacity(64);
        let mut utf16_starts = Vec::with_capacity(64);
        starts.push(0);
        utf16_starts.push(0);
        let mut utf16 = 0usize;
        for (i, ch) in content.char_indices() {
            utf16 += ch.len_utf16();
            if ch == '\n' {
                starts.push(i + 1);
                utf16_starts.push(utf16);
            }
        }
        Self {
            starts,
            utf16_starts,
        }
    }

    /// UTF-16 code-unit offset for a byte `offset` — line-prefix lookup plus
    /// a walk of that line only.
    pub(super) fn offset_to_utf16(&self, content: &str, offset: usize) -> usize {
        let (line, _) = self.line_col_for_offset(offset);
        let start = self.line_start(line);
        let mut utf16 = self.utf16_starts.get(line).copied().unwrap_or(0);
        for (i, ch) in content[start..].char_indices() {
            if start + i >= offset {
                break;
            }
            utf16 += ch.len_utf16();
        }
        utf16
    }

    /// Byte offset for a UTF-16 code-unit offset; always a char boundary
    /// (a mid-surrogate input rounds down to the char start).
    pub(super) fn offset_from_utf16(&self, content: &str, utf16_offset: usize) -> usize {
        let line = self
            .utf16_starts
            .partition_point(|&s| s <= utf16_offset)
            .saturating_sub(1);
        let start = self.line_start(line);
        let mut utf16 = self.utf16_starts.get(line).copied().unwrap_or(0);
        for (i, ch) in content[start..].char_indices() {
            if utf16 >= utf16_offset {
                return start + i;
            }
            utf16 += ch.len_utf16();
        }
        content.len()
    }

    pub(super) fn line_count(&self) -> usize {
        self.starts.len()
    }

    pub(super) fn line_start(&self, line: usize) -> usize {
        self.starts
            .get(line)
            .copied()
            .unwrap_or_else(|| *self.starts.last().unwrap_or(&0))
    }

    pub(super) fn line_end(&self, content: &str, line: usize) -> usize {
        self.starts
            .get(line + 1)
            .map(|next| next - 1)
            .unwrap_or(content.len())
    }

    pub(super) fn line_col_for_offset(&self, offset: usize) -> (usize, usize) {
        let line = self.starts.partition_point(|&s| s <= offset);
        let line = if line > 0 { line - 1 } else { 0 };
        let start = self.starts.get(line).copied().unwrap_or(0);
        (line, offset.saturating_sub(start))
    }
}

/// Wiring for a same-buffer split: two `SqlEditor` entities presenting one
/// document. DOCUMENT state (text, revision, undo/redo stacks, diagnostics)
/// is mirrored from whichever view mutated it, driven by `doc_epoch`; VIEW
/// state (cursor, selection, extra cursors, scroll, find bar, completion
/// popup, folds, focus) stays per-entity. The buffer-derived caches
/// (`line_index_cache`, `shape_cache`, `max_display_cols_cache`) are `Rc`s
/// shared between the two views, so parser/shaping work stays per-document.
pub(super) struct SharedDocumentPeer {
    /// The other view. Weak: the split owns the second view's lifetime.
    pub(super) peer: gpui::WeakEntity<SqlEditor>,
    /// True on the tab's canonical editor. Only the primary forwards
    /// app-pushed settings (prefs, fonts, completion inputs) to the view,
    /// which is what keeps forwarding loop-free.
    pub(super) primary: bool,
    /// The peer's `doc_epoch` as of the last adoption; an unchanged epoch
    /// makes the sync a no-op compare.
    pub(super) adopted_epoch: u64,
    /// Keep the observe + Change subscriptions on the peer alive for the
    /// link's lifetime; dropped (unsubscribing) on detach.
    pub(super) _subscriptions: [Subscription; 2],
}

#[derive(Clone)]
pub(super) enum SnapshotPayload {
    /// Whole buffer as it was before the edit. Used for grouped typing and
    /// wholesale replacements that have no single splice range.
    Full(SharedString),
    /// Inverse splice: restore `replaced_text` into `range` in the post-edit
    /// buffer (the span the edit occupied after it landed).
    Delta {
        range: Range<usize>,
        replaced_text: SharedString,
    },
}

impl SnapshotPayload {
    pub(super) fn retained_bytes(&self) -> usize {
        match self {
            Self::Full(content) => content.len(),
            Self::Delta { replaced_text, .. } => replaced_text.len(),
        }
    }
}

#[derive(Clone)]
pub(super) struct EditorSnapshot {
    pub(super) payload: SnapshotPayload,
    pub(super) cursor: usize,
    pub(super) selection_anchor: Option<usize>,
    /// Extra (multi-cursor) carets at snapshot time. Additive: snapshots
    /// taken before multi-cursor mode existed simply carry an empty vec, so
    /// undoing across them behaves exactly as before.
    pub(super) extra_cursors: Vec<ExtraCursor>,
}

/// Hard cap on secondary carets: adds beyond this are dropped.
pub(super) const MAX_EXTRA_CURSORS: usize = 99;

/// A secondary caret in multi-cursor mode, mirroring the primary's
/// `cursor` / `selection_anchor` pair. The primary caret stays in
/// `SqlEditor::cursor` + `SqlEditor::selection_anchor`, so every existing
/// code path that reads those keeps meaning "the primary".
///
/// Invariants (enforced by `normalize_extra_cursors`): ascending caret
/// order, no duplicate carets, never on top of the primary caret, at most
/// [`MAX_EXTRA_CURSORS`] entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ExtraCursor {
    pub(super) cursor: usize,
    /// Selection anchor for this caret; `None` = plain caret.
    pub(super) anchor: Option<usize>,
}

impl ExtraCursor {
    pub(super) fn caret(cursor: usize) -> Self {
        Self {
            cursor,
            anchor: None,
        }
    }

    pub(super) fn selection(&self) -> Option<Range<usize>> {
        self.anchor.map(|a| {
            if a < self.cursor {
                a..self.cursor
            } else {
                self.cursor..a
            }
        })
    }

    /// The range an edit at this caret replaces: its selection, or the empty
    /// range at the caret.
    pub(super) fn edit_range(&self) -> Range<usize> {
        self.selection().unwrap_or(self.cursor..self.cursor)
    }
}

/// Active LSP-snippet walk. Ranges are absolute byte offsets into `content`.
/// `index` is the current stop; `0`-numbered stops are sorted last. `span` is
/// the inserted body — an edit outside it ends the session.
pub(super) struct SnippetSession {
    pub(super) stops: Vec<crate::snippets::SnippetStop>,
    pub(super) index: usize,
    pub(super) span: Range<usize>,
}

/// Kind of single-char edit that can extend an open undo group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UndoGroupKind {
    Insert,
    DeleteBackward,
    DeleteForward,
}

/// An open undo group: the next single-char edit of `kind` at byte offset
/// `at` continues the group (no new snapshot). `whitespace` tracks the char
/// class of inserts so a word→space transition starts a new group, giving
/// word-granular undo like mainstream editors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct UndoGroupState {
    pub(super) kind: UndoGroupKind,
    pub(super) at: usize,
    pub(super) whitespace: bool,
}

pub(super) const MAX_UNDO_STACK: usize = 200;
pub(super) const UNDO_BYTE_BUDGET: usize = 64 * 1024 * 1024;
