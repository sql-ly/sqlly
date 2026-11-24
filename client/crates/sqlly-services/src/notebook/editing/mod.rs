//! Editing operations on a notebook document that the app's command mode,
//! clipboard, find bar and undo stack are built on. Pure, like the rest of
//! the core, so the browser build and tools can reuse them.
//!
//! - [`history`]: structural undo and redo as shared snapshots.
//! - [`fragment`]: cells on the clipboard (native fragment, SQL, Markdown)
//!   and plain text pasted as cells.
//! - [`structure`]: split, merge, block moves and heading sections.
//! - [`find`]: find and replace across sources, names, the title and spec
//!   text, with output values searchable on request.

/// Cell `extra` keys for view state kept in the file: a bookmark, a folded
/// heading and a pinned output. They never change what a cell runs.
pub const BOOKMARK_KEY: &str = "bookmarked";
pub const FOLDED_KEY: &str = "folded";
pub const PINNED_KEY: &str = "output_pinned";
pub const VIEW_KEYS: [&str; 3] = [BOOKMARK_KEY, FOLDED_KEY, PINNED_KEY];

pub mod find;
pub mod fragment;
pub mod history;
pub mod structure;

pub use find::{FindField, FindMatch, FindOptions};
pub use fragment::Fragment;
pub use history::{History, Restore, Snapshot};
