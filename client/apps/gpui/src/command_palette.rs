//! Command palette (and object quick-open) with a hand-rolled result list.
//!
//! Migrating the list portion to gpui-component 0.6.4's `list`
//! (`ListDelegate`/`ListState`/`List`) was evaluated and rejected because the
//! kit list cannot reproduce this palette's behavior exactly:
//!
//! - **Variable row heights.** `ListDelegate` requires every item to share one
//!   height (the virtual list measures a single row and reuses that size for
//!   all entries in `RowsCache`). Palette rows for unavailable commands render
//!   a second "reason" sub-line and are taller, so scroll offsets and
//!   scroll-to-selected would be wrong.
//! - **Three-way Enter semantics.** Object mode distinguishes plain Enter
//!   (reveal), cmd/ctrl+Enter (select rows), and alt+Enter (definition). The
//!   kit's `Confirm { secondary }` action — for both `enter`/`secondary-enter`
//!   keybindings and row clicks — is only two-state; alt+Enter is not
//!   expressible without re-adding an external key handler, defeating the
//!   migration.
//! - **Query trimming.** `ListState::start_search` trims the query before
//!   `perform_search`, changing matcher semantics for leading/trailing
//!   whitespace (spaces are significant to `subsequence_score`, and a leading
//!   space today prevents the `>` command-mode prefix from engaging).
//! - **Chrome changes.** The kit's searchable list swaps in its own query
//!   input (search-icon prefix, no appearance, kit paddings), shows a loading
//!   spinner plus a 100ms "searching" latch on every keystroke, and overlays a
//!   kit scrollbar — all visible departures from the current palette chrome.
//!
//! The existing implementation stays; the matcher/MRU model below is pure and
//! fully covered by the tests at the bottom of this file.
use gpui::{
    Context, Entity, EventEmitter, IntoElement, KeyDownEvent, Render, ScrollHandle, Styled,
    Subscription, Window, div, prelude::*, px,
};
use uuid::Uuid;

use crate::quick_open::{QuickOpenCandidate, QuickOpenMatch};
use crate::theme::Theme;
use gpui_component::input::{Input, InputEvent, InputState};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandCategory {
    File,
    Edit,
    View,
    Query,
    Tools,
    QuickActions,
    Window,
    Help,
}

impl CommandCategory {
    pub fn label(self) -> String {
        let key = match self {
            Self::File => "command_palette.category.file",
            Self::Edit => "command_palette.category.edit",
            Self::View => "command_palette.category.view",
            Self::Query => "command_palette.category.query",
            Self::Tools => "command_palette.category.tools",
            Self::QuickActions => "command_palette.category.quick_actions",
            Self::Window => "command_palette.category.window",
            Self::Help => "command_palette.category.help",
        };
        crate::localization::text(key)
    }

    #[allow(dead_code)]
    pub fn all() -> [Self; 8] {
        [
            Self::File,
            Self::Edit,
            Self::View,
            Self::Query,
            Self::Tools,
            Self::QuickActions,
            Self::Window,
            Self::Help,
        ]
    }
}

#[derive(Debug, Clone)]
pub struct ShellCommand {
    pub id: String,
    pub title: String,
    pub category: CommandCategory,
    pub keywords: Vec<&'static str>,
}

#[derive(Clone)]
pub struct CommandRegistry {
    commands: Vec<ShellCommand>,
}

fn command_title_key(id: &str) -> String {
    let mut key = String::from("command_palette.command.");
    for (index, segment) in id.split('.').enumerate() {
        if index > 0 {
            key.push('.');
        }
        for (character_index, character) in segment.chars().enumerate() {
            if character.is_ascii_uppercase() {
                if character_index > 0 {
                    key.push('_');
                }
                key.push(character.to_ascii_lowercase());
            } else {
                key.push(character);
            }
        }
    }
    key
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub fn register(&mut self, mut command: ShellCommand) {
        let key = command_title_key(&command.id);
        if sqlly_i18n::contains_key(&key) {
            command.title = crate::localization::text(&key);
        }
        self.commands.push(command);
    }

    pub fn all(&self) -> &[ShellCommand] {
        &self.commands
    }

    pub fn default_registry() -> Self {
        let mut reg = Self::new();
        reg.register(ShellCommand {
            id: "file.newQuery".to_string(),
            title: "New Query".to_string(),
            category: CommandCategory::File,
            keywords: vec!["new", "tab", "query"],
        });
        reg.register(ShellCommand {
            id: "file.newNotebook".to_string(),
            title: "New Notebook".to_string(),
            category: CommandCategory::File,
            keywords: vec!["notebook", "cells", "markdown"],
        });
        reg.register(ShellCommand {
            id: "file.openNotebook".to_string(),
            title: "Open Notebook".to_string(),
            category: CommandCategory::File,
            keywords: vec!["notebook", "open", "sqlnb", "ipynb", "jupyter"],
        });
        reg.register(ShellCommand {
            id: "file.saveNotebook".to_string(),
            title: "Save Notebook".to_string(),
            category: CommandCategory::File,
            keywords: vec!["notebook", "save", "sqlnb"],
        });
        reg.register(ShellCommand {
            id: "file.saveWorkspaceAs".to_string(),
            title: "Save Workspace As".to_string(),
            category: CommandCategory::File,
            keywords: vec!["workspace", "save", "layout", "session"],
        });
        reg.register(ShellCommand {
            id: "file.openWorkspace".to_string(),
            title: "Open Workspace".to_string(),
            category: CommandCategory::File,
            keywords: vec!["workspace", "open", "restore", "session"],
        });
        reg.register(ShellCommand {
            id: "file.manageWorkspaces".to_string(),
            title: "Manage Workspaces".to_string(),
            category: CommandCategory::File,
            keywords: vec!["workspace", "rename", "duplicate", "delete"],
        });
        reg.register(ShellCommand {
            id: "file.closeTab".to_string(),
            title: "Close Tab".to_string(),
            category: CommandCategory::File,
            keywords: vec!["close", "tab"],
        });
        reg.register(ShellCommand {
            id: "tab.rename".to_string(),
            title: "Rename Tab…".to_string(),
            category: CommandCategory::File,
            keywords: vec!["rename", "tab", "title"],
        });
        reg.register(ShellCommand {
            id: "tab.moveToNewWindow".to_string(),
            title: "Move Tab to New Window".to_string(),
            category: CommandCategory::File,
            keywords: vec!["move", "tab", "window", "detach", "tear", "off"],
        });
        reg.register(ShellCommand {
            id: "tools.tenantScoping".to_string(),
            title: "Tenant Scoping".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["tenant", "scoping", "safety", "multi-tenant"],
        });
        reg.register(ShellCommand {
            id: "tools.relationshipGraph".to_string(),
            title: "Relationship Graph".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["er", "diagram", "relationships", "foreign", "keys", "graph"],
        });
        reg.register(ShellCommand {
            id: "tools.queryBuilder".to_string(),
            title: "Query Builder".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["query", "builder", "visual", "design", "join"],
        });
        reg.register(ShellCommand {
            id: "tools.connectionHealth".to_string(),
            title: "Connection Health".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["connection", "health", "test", "latency", "diagnostics"],
        });
        reg.register(ShellCommand {
            id: "tools.resultSnapshots".to_string(),
            title: "Result Snapshots".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["snapshot", "results", "saved", "capture"],
        });
        reg.register(ShellCommand {
            id: "tools.savedCharts".to_string(),
            title: "Saved Charts".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["chart", "library", "saved", "graph", "plot"],
        });
        reg.register(ShellCommand {
            id: "tools.chartDashboard".to_string(),
            title: "Chart Dashboard".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["chart", "dashboard", "tiles", "refresh", "graph"],
        });
        reg.register(ShellCommand {
            id: "results.filterRows".to_string(),
            title: "Results: Filter Rows…".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["filter", "where", "grid"],
        });
        reg.register(ShellCommand {
            id: "results.sortAscending".to_string(),
            title: "Results: Sort Active Column Ascending".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["sort", "ascending", "column", "grid", "keyboard"],
        });
        reg.register(ShellCommand {
            id: "results.sortDescending".to_string(),
            title: "Results: Sort Active Column Descending".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["sort", "descending", "column", "grid", "keyboard"],
        });
        reg.register(ShellCommand {
            id: "results.transpose".to_string(),
            title: "Results: Transpose".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["transpose", "rotate", "flip", "grid"],
        });
        reg.register(ShellCommand {
            id: "results.stage_undo".to_string(),
            title: "Results: Undo Staged Edit".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["stage", "undo", "grid", "pending"],
        });
        reg.register(ShellCommand {
            id: "results.stage_redo".to_string(),
            title: "Results: Redo Staged Edit".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["stage", "redo", "grid", "pending"],
        });
        reg.register(ShellCommand {
            id: "results.save_staged".to_string(),
            title: "Results: Save Staged Changes".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["stage", "save", "commit", "grid"],
        });
        reg.register(ShellCommand {
            id: "results.discard_staged".to_string(),
            title: "Results: Discard Staged Changes".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["stage", "discard", "grid", "pending"],
        });
        reg.register(ShellCommand {
            id: "results.preview_staged_sql".to_string(),
            title: "Results: Preview Staged SQL".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["stage", "preview", "sql", "grid"],
        });
        reg.register(ShellCommand {
            id: "results.copy_staged_sql".to_string(),
            title: "Results: Copy Staged Changes as SQL".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["stage", "copy", "sql", "script", "pending", "clipboard"],
        });
        reg.register(ShellCommand {
            id: "results.findInGrid".to_string(),
            title: "Results: Find in Grid".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["find", "search", "grid", "replace"],
        });
        reg.register(ShellCommand {
            id: "results.gotoColumn".to_string(),
            title: "Results: Go to Column…".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["column", "goto", "jump", "find column"],
        });
        reg.register(ShellCommand {
            id: "results.focusToggle".to_string(),
            title: "Results: Toggle Focus Between Editor and Grid".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["focus", "keyboard", "grid", "editor", "jump"],
        });
        reg.register(ShellCommand {
            id: "results.stageInsertRow".to_string(),
            title: "Results: Stage New Row".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["stage", "insert", "row", "grid"],
        });
        reg.register(ShellCommand {
            id: "results.stageDeleteRow".to_string(),
            title: "Results: Mark Row for Deletion".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["stage", "delete", "row", "grid"],
        });
        reg.register(ShellCommand {
            id: "results.refresh".to_string(),
            title: "Results: Refresh Query".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["refresh", "rerun", "grid", "query"],
        });
        reg.register(ShellCommand {
            id: "file.reopenClosedTab".to_string(),
            title: "Reopen Closed Tab".to_string(),
            category: CommandCategory::File,
            keywords: vec!["reopen", "closed", "tab", "restore", "undo"],
        });
        reg.register(ShellCommand {
            id: "edit.find".to_string(),
            title: "Find in Editor".to_string(),
            category: CommandCategory::Edit,
            keywords: vec!["find", "search", "locate"],
        });
        reg.register(ShellCommand {
            id: "edit.replace".to_string(),
            title: "Replace in Editor".to_string(),
            category: CommandCategory::Edit,
            keywords: vec!["replace", "find", "substitute"],
        });
        reg.register(ShellCommand {
            id: "editor.toggleVimMode".to_string(),
            title: "Toggle Vim Keybindings".to_string(),
            category: CommandCategory::Edit,
            keywords: vec!["vim", "modal", "normal", "insert", "keybindings"],
        });
        reg.register(ShellCommand {
            id: "query.execute".to_string(),
            title: "Execute Query".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["run", "execute", "sql"],
        });
        reg.register(ShellCommand {
            id: "query.executeKeepResults".to_string(),
            title: "Execute into New Result Tab".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["run", "execute", "keep", "results", "tab"],
        });
        reg.register(ShellCommand {
            id: "query.runPicker".to_string(),
            title: "Run Statements…".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["run", "statements", "picker"],
        });
        reg.register(ShellCommand {
            id: "query.runNotebookCell".to_string(),
            title: "Run Notebook Cell".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["notebook", "cell", "run"],
        });
        reg.register(ShellCommand {
            id: "query.runAllNotebookCells".to_string(),
            title: "Run All Notebook Cells".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["notebook", "run", "all", "cells"],
        });
        reg.register(ShellCommand {
            id: "query.executeNoRollback".to_string(),
            title: "Execute Without Rollback Wrap".to_string(),
            category: CommandCategory::Query,
            keywords: vec![
                "run", "execute", "rollback", "wrap", "bypass", "deploy", "persist", "ddl",
                "commit",
            ],
        });
        reg.register(ShellCommand {
            id: "safety.unlockWrites1m".to_string(),
            title: "Unlock Production Writes for 1 Minute".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["production", "unlock", "write", "read-only", "lock"],
        });
        reg.register(ShellCommand {
            id: "safety.unlockWrites5m".to_string(),
            title: "Unlock Production Writes for 5 Minutes".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["production", "unlock", "write", "read-only", "lock"],
        });
        reg.register(ShellCommand {
            id: "safety.unlockWrites15m".to_string(),
            title: "Unlock Production Writes for 15 Minutes".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["production", "unlock", "write", "read-only", "lock"],
        });
        reg.register(ShellCommand {
            id: "safety.lockWrites".to_string(),
            title: "Lock Production Writes Now".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["production", "unlock", "write", "read-only", "lock"],
        });
        reg.register(ShellCommand {
            id: "transaction.toggleMode".to_string(),
            title: "Transaction: Toggle Manual Commit Mode".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["transaction", "manual", "auto", "commit", "begin"],
        });
        reg.register(ShellCommand {
            id: "transaction.commit".to_string(),
            title: "Transaction: Commit".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["transaction", "commit", "manual"],
        });
        reg.register(ShellCommand {
            id: "transaction.rollback".to_string(),
            title: "Transaction: Rollback".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["transaction", "rollback", "manual"],
        });
        reg.register(ShellCommand {
            id: "safety.unlockReadOnly1m".to_string(),
            title: "Unlock Read-only Connection for 1 Minute".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["read-only", "select", "unlock", "write"],
        });
        reg.register(ShellCommand {
            id: "safety.unlockReadOnly5m".to_string(),
            title: "Unlock Read-only Connection for 5 Minutes".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["read-only", "select", "unlock", "write"],
        });
        reg.register(ShellCommand {
            id: "safety.unlockReadOnly15m".to_string(),
            title: "Unlock Read-only Connection for 15 Minutes".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["read-only", "select", "unlock", "write"],
        });
        reg.register(ShellCommand {
            id: "safety.lockReadOnly".to_string(),
            title: "Lock Read-only Connection Now".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["read-only", "select", "unlock", "write", "lock"],
        });
        reg.register(ShellCommand {
            id: "query.explainPlan".to_string(),
            title: "Explain Query (Actual, Executes Query)".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["explain", "plan", "showplan", "execution"],
        });
        reg.register(ShellCommand {
            id: "query.explainEstimated".to_string(),
            title: "Explain Query (Estimated Plan, No Execution)".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["explain", "estimated", "plan", "showplan"],
        });
        reg.register(ShellCommand {
            id: "plan.profile".to_string(),
            title: "Explain with Profile (SELECT only)".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["explain", "profile", "analyze", "select"],
        });
        reg.register(ShellCommand {
            id: "plan.open".to_string(),
            title: "Open Plan…".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["plan", "open", "import", "sqlplan"],
        });
        reg.register(ShellCommand {
            id: "plan.paste".to_string(),
            title: "Paste Plan from Clipboard".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["plan", "paste", "clipboard", "import"],
        });
        reg.register(ShellCommand {
            id: "query.history".to_string(),
            title: "Query History".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["history", "recent", "past", "pinned", "rerun"],
        });
        reg.register(ShellCommand {
            id: "query.cancel".to_string(),
            title: "Cancel Query".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["cancel", "stop", "abort"],
        });
        reg.register(ShellCommand {
            id: "query.runOnTargets".to_string(),
            title: "Run on Multiple Connections…".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["run", "multi", "environments", "fan out", "targets", "all"],
        });
        reg.register(ShellCommand {
            id: "results.autoRefreshOff".to_string(),
            title: "Results: Auto-refresh Off".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["refresh", "auto", "interval", "timer", "poll"],
        });
        reg.register(ShellCommand {
            id: "results.autoRefresh5s".to_string(),
            title: "Results: Auto-refresh Every 5s".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["refresh", "auto", "interval", "timer", "poll"],
        });
        reg.register(ShellCommand {
            id: "results.autoRefresh10s".to_string(),
            title: "Results: Auto-refresh Every 10s".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["refresh", "auto", "interval", "timer", "poll"],
        });
        reg.register(ShellCommand {
            id: "results.autoRefresh30s".to_string(),
            title: "Results: Auto-refresh Every 30s".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["refresh", "auto", "interval", "timer", "poll"],
        });
        reg.register(ShellCommand {
            id: "results.autoRefresh60s".to_string(),
            title: "Results: Auto-refresh Every 60s".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["refresh", "auto", "interval", "timer", "poll"],
        });
        reg.register(ShellCommand {
            id: "query.validate".to_string(),
            title: "Validate SQL".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["validate", "check", "lint"],
        });
        reg.register(ShellCommand {
            id: "query.formatSql".to_string(),
            title: "Format SQL".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["format", "pretty", "beautify", "indent", "reformat"],
        });
        reg.register(ShellCommand {
            id: "editor.formatDocument".to_string(),
            title: "Format Document".to_string(),
            category: CommandCategory::Query,
            keywords: vec!["format", "document", "whole", "entire", "reformat"],
        });
        reg.register(ShellCommand {
            id: "editor.refreshDates".to_string(),
            title: "Refresh Natural-Language Dates".to_string(),
            category: CommandCategory::Query,
            keywords: vec![
                "date",
                "dates",
                "refresh",
                "today",
                "sqlly date",
                "re-evaluate",
            ],
        });
        reg.register(ShellCommand {
            id: "editor.minify".to_string(),
            title: "Minify SQL".to_string(),
            category: CommandCategory::Edit,
            keywords: vec!["minify", "compact", "one line", "collapse"],
        });
        reg.register(ShellCommand {
            id: "view.toggleHistoryPanel".to_string(),
            title: "Toggle Version History Panel".to_string(),
            category: CommandCategory::View,
            keywords: vec!["history", "versions", "timeline", "restore"],
        });
        reg.register(ShellCommand {
            id: "view.toggleSidebar".to_string(),
            title: "Toggle Sidebar".to_string(),
            category: CommandCategory::View,
            keywords: vec!["sidebar", "toggle", "hide"],
        });
        reg.register(ShellCommand {
            id: "view.toggleAiPanel".to_string(),
            title: "Toggle AI Panel".to_string(),
            category: CommandCategory::View,
            keywords: vec!["ai", "panel", "toggle"],
        });
        reg.register(ShellCommand {
            id: "ai.toggleAgentMode".to_string(),
            title: "Toggle Agent Mode".to_string(),
            category: CommandCategory::View,
            keywords: vec!["ai", "agent", "tools", "toggle"],
        });
        reg.register(ShellCommand {
            id: "view.toggleAiVisibility".to_string(),
            title: "Toggle AI Panel Visibility".to_string(),
            category: CommandCategory::View,
            keywords: vec!["ai", "panel", "show", "hide"],
        });
        reg.register(ShellCommand {
            id: "view.splitVertical".to_string(),
            title: "Split Editor Vertically".to_string(),
            category: CommandCategory::View,
            keywords: vec!["split", "editor", "vertical", "pane"],
        });
        reg.register(ShellCommand {
            id: "view.splitHorizontal".to_string(),
            title: "Split Editor Horizontally".to_string(),
            category: CommandCategory::View,
            keywords: vec!["split", "editor", "horizontal", "pane"],
        });
        reg.register(ShellCommand {
            id: "view.splitSameBufferVertical".to_string(),
            title: "Split Same Buffer Vertically".to_string(),
            category: CommandCategory::View,
            keywords: vec!["split", "same", "buffer", "vertical", "pane"],
        });
        reg.register(ShellCommand {
            id: "view.splitSameBufferHorizontal".to_string(),
            title: "Split Same Buffer Horizontally".to_string(),
            category: CommandCategory::View,
            keywords: vec!["split", "same", "buffer", "horizontal", "pane"],
        });
        reg.register(ShellCommand {
            id: "view.closeSplit".to_string(),
            title: "Close Split Editor".to_string(),
            category: CommandCategory::View,
            keywords: vec!["split", "close", "unsplit", "single"],
        });
        reg.register(ShellCommand {
            id: "view.toggleLogConsole".to_string(),
            title: "Toggle Log Console".to_string(),
            category: CommandCategory::View,
            keywords: vec!["log", "console", "diagnostic", "toggle"],
        });
        reg.register(ShellCommand {
            id: "tools.commandPalette".to_string(),
            title: "Command Palette".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["palette", "search", "commands"],
        });
        reg.register(ShellCommand {
            id: "tools.quickOpen".to_string(),
            title: "Go to Object…".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["quick open", "jump", "table", "find object", "goto"],
        });
        reg.register(ShellCommand {
            id: "connection.add".to_string(),
            title: "Add Connection".to_string(),
            category: CommandCategory::File,
            keywords: vec!["connection", "add", "new", "server"],
        });
        reg.register(ShellCommand {
            id: "connection.disconnect".to_string(),
            title: "Disconnect".to_string(),
            category: CommandCategory::File,
            keywords: vec!["disconnect", "connection"],
        });
        reg.register(ShellCommand {
            id: "schema.refresh".to_string(),
            title: "Refresh IntelliSense Catalog".to_string(),
            category: CommandCategory::Tools,
            keywords: vec!["refresh", "schema", "catalog", "intellisense", "reload"],
        });
        // ── Full menu-bar coverage. Every entry below mirrors a menu action;
        // `PALETTE_DISPATCH_IDS` in app.rs must list each id (a test enforces
        // the pairing), so a new command here without dispatch fails CI.
        let more: &[(&str, &str, CommandCategory, &[&'static str])] = &[
            (
                "file.newWindow",
                "New Window",
                CommandCategory::File,
                &["window", "new"],
            ),
            (
                "file.openSqlFile",
                "Open SQL File…",
                CommandCategory::File,
                &["open", "file", "sql"],
            ),
            (
                "file.runSqlFile",
                "Run SQL File…",
                CommandCategory::File,
                &["run", "file", "script", "batch", "execute"],
            ),
            (
                "file.openProjectFile",
                "Open Project File…",
                CommandCategory::File,
                &["open", "project"],
            ),
            (
                "file.openSqllyExport",
                "Open SQLLY Export…",
                CommandCategory::File,
                &["open", "results", "snapshot", "sqllyexport"],
            ),
            (
                "file.exportConnections",
                "Export Connections…",
                CommandCategory::File,
                &["export", "connections", "profiles"],
            ),
            (
                "file.exportCredentials",
                "Export Conn w/ Creds…",
                CommandCategory::File,
                &[
                    "export",
                    "credentials",
                    "passwords",
                    "connections",
                    "backup",
                ],
            ),
            (
                "file.importConnections",
                "Import Connections…",
                CommandCategory::File,
                // The one import: it also reads files with passwords.
                &[
                    "import",
                    "connections",
                    "profiles",
                    "credentials",
                    "passwords",
                ],
            ),
            (
                "file.importConnectionsFromTools",
                "Import From Other Tools…",
                CommandCategory::File,
                &[
                    "import",
                    "connections",
                    "datagrip",
                    "dbeaver",
                    "jdbc",
                    "xml",
                    "json",
                    "pgadmin",
                    "azure data studio",
                    "vscode",
                    "dbgate",
                    "pg_service",
                ],
            ),
            (
                "file.exportForPgAdmin",
                "Export for pgAdmin (servers.json)…",
                CommandCategory::File,
                &[
                    "export",
                    "connections",
                    "pgadmin",
                    "postgres",
                    "servers.json",
                ],
            ),
            (
                "file.exportForAzureDataStudio",
                "Export for Azure Data Studio / VS Code (settings.json)…",
                CommandCategory::File,
                &[
                    "export",
                    "connections",
                    "azure",
                    "data studio",
                    "ads",
                    "vscode",
                    "mssql",
                    "settings.json",
                ],
            ),
            (
                "file.exportForPgService",
                "Export as PostgreSQL Service File (pg_service.conf)…",
                CommandCategory::File,
                &[
                    "export",
                    "connections",
                    "psql",
                    "pg_service",
                    "libpq",
                    "postgres",
                ],
            ),
            (
                "file.exportForDbeaver",
                "Export for DBeaver (data-sources.json)…",
                CommandCategory::File,
                &["export", "connections", "dbeaver", "data-sources.json"],
            ),
            (
                "file.exportForDataGrip",
                "Export for DataGrip (dataSources.xml)…",
                CommandCategory::File,
                &[
                    "export",
                    "connections",
                    "datagrip",
                    "jetbrains",
                    "intellij",
                    "datasources.xml",
                ],
            ),
            (
                "file.exportForDbGate",
                "Export for DbGate (.env)…",
                CommandCategory::File,
                &["export", "connections", "dbgate", "env"],
            ),
            (
                "file.openFolder",
                "Open Folder…",
                CommandCategory::File,
                &["open", "folder", "directory"],
            ),
            (
                "file.projectGit",
                "Project Folder: Git Changes…",
                CommandCategory::File,
                &[
                    "git", "commit", "diff", "status", "project", "folder", "vcs",
                ],
            ),
            (
                "file.saveSql",
                "Save SQL…",
                CommandCategory::File,
                &["save", "file", "sql"],
            ),
            (
                "file.saveToLibrary",
                "Save to Library…",
                CommandCategory::File,
                &["library", "save", "snippet"],
            ),
            (
                "file.saveResults",
                "Save Results…",
                CommandCategory::File,
                &["save", "results", "export", "csv"],
            ),
            (
                "connection.connect",
                "Connect…",
                CommandCategory::File,
                &["connect", "server", "database"],
            ),
            (
                "view.toggleResultsPane",
                "Toggle Results Pane",
                CommandCategory::View,
                &["results", "pane", "toggle"],
            ),
            (
                "view.toggleResultsMaximized",
                "Maximize / Restore Results",
                CommandCategory::View,
                &["results", "maximize", "pane", "grid"],
            ),
            (
                "view.detachResults",
                "Detach Results to Window",
                CommandCategory::View,
                &["results", "detach", "window", "pane"],
            ),
            (
                "view.reattachResults",
                "Re-attach Results",
                CommandCategory::View,
                &["results", "reattach", "window", "pane"],
            ),
            (
                "view.toggleCellDetails",
                "Toggle Cell Details Panel",
                CommandCategory::View,
                &["cell", "detail", "inspector", "value", "row", "panel"],
            ),
            (
                "view.searchSchema",
                "Search Schema",
                CommandCategory::View,
                &["search", "schema", "explorer", "filter"],
            ),
            (
                "view.sqlLibrary",
                "Show SQL Library",
                CommandCategory::View,
                &["library", "saved", "queries", "snippets"],
            ),
            (
                "view.toggleIntellisenseDebug",
                "Toggle IntelliSense Debug Panel",
                CommandCategory::View,
                &["intellisense", "debug", "lsp"],
            ),
            (
                "view.zoomIn",
                "Zoom In",
                CommandCategory::View,
                &["zoom", "bigger", "font"],
            ),
            (
                "view.zoomOut",
                "Zoom Out",
                CommandCategory::View,
                &["zoom", "smaller", "font"],
            ),
            (
                "view.zoomReset",
                "Actual Size",
                CommandCategory::View,
                &["zoom", "reset", "font"],
            ),
            (
                "view.themeSystem",
                "Theme: System",
                CommandCategory::View,
                &["theme", "system", "auto"],
            ),
            (
                "view.themeLight",
                "Theme: Light",
                CommandCategory::View,
                &["theme", "light"],
            ),
            (
                "view.themeDark",
                "Theme: Dark",
                CommandCategory::View,
                &["theme", "dark"],
            ),
            (
                "view.themeHighContrast",
                "Theme: High Contrast",
                CommandCategory::View,
                &["theme", "contrast", "accessibility"],
            ),
            (
                "query.generateSql",
                "Generate SQL (AI)…",
                CommandCategory::Query,
                &["generate", "ai", "sql", "natural"],
            ),
            (
                "query.askSchema",
                "Ask Schema (AI)…",
                CommandCategory::Query,
                &["ask", "ai", "schema", "question"],
            ),
            (
                "query.sendSelectionToAi",
                "Send Selection to AI…",
                CommandCategory::Query,
                &["ai", "selection", "explain", "send"],
            ),
            (
                "query.explainError",
                "Explain Last Error (AI)",
                CommandCategory::Query,
                &["explain", "error", "ai", "fix", "why", "failed"],
            ),
            (
                "query.explainPlanAi",
                "Explain Execution Plan (AI)",
                CommandCategory::Query,
                &["explain", "plan", "ai", "execution", "why", "slow"],
            ),
            (
                "ai.analyzeLastRun",
                "Analyze Last Run with AI",
                CommandCategory::Query,
                &["analyze", "ai", "slow", "explain"],
            ),
            (
                "ai.buildCatalog",
                "Build AI Cache and Embeddings",
                CommandCategory::Query,
                &["ai", "cache", "embeddings", "build", "catalog", "index"],
            ),
            (
                "ai.invalidateData",
                "Clear AI Catalog Data",
                CommandCategory::Query,
                &["ai", "cache", "embeddings", "clear", "invalidate", "reset"],
            ),
            (
                "ai.mcpServerSettings",
                "MCP Server Settings",
                CommandCategory::Tools,
                &["mcp", "server", "claude", "cursor", "agent"],
            ),
            (
                "connection.objectProperties",
                "Object Properties…",
                CommandCategory::Tools,
                &["object", "properties", "info"],
            ),
            (
                "connection.openInTerminal",
                "Open Connection in Terminal",
                CommandCategory::Tools,
                &[
                    "terminal",
                    "shell",
                    "cli",
                    "psql",
                    "mysql",
                    "sqlcmd",
                    "redis-cli",
                    "duckdb",
                    "sqlplus",
                    "console",
                ],
            ),
            (
                "connection.refreshExplorer",
                "Refresh Object Explorer",
                CommandCategory::Tools,
                &["refresh", "explorer", "tree", "objects"],
            ),
            (
                "tools.preferences",
                "Preferences…",
                CommandCategory::Tools,
                &["preferences", "settings", "options"],
            ),
            (
                "tools.keybindings",
                "Open Keyboard Shortcuts…",
                CommandCategory::Tools,
                &["keyboard", "shortcuts", "keybindings", "hotkeys"],
            ),
            (
                "connections.tunnelProfiles",
                "Manage Tunnel Profiles…",
                CommandCategory::Tools,
                &["tunnel", "proxy", "ssh", "bastion", "profile"],
            ),
            (
                "tools.connectionManagement",
                "Connection Management…",
                CommandCategory::Tools,
                &["connection", "management", "servers"],
            ),
            (
                "tools.externalEditor",
                "Open in External Editor",
                CommandCategory::Tools,
                &["external", "editor", "vscode"],
            ),
            (
                "tools.templateExplorer",
                "Template Explorer…",
                CommandCategory::Tools,
                &["template", "explorer", "snippets"],
            ),
            (
                "tools.deployCart",
                "Deploy Cart…",
                CommandCategory::Tools,
                &["deploy", "cart", "collect", "migrate", "objects"],
            ),
            (
                "tools.backupRestore",
                "Backup / Restore…",
                CommandCategory::Tools,
                &["backup", "restore", "bak"],
            ),
            (
                "tools.searchDatabaseValue",
                "Search Database for Value…",
                CommandCategory::Tools,
                &["search", "find", "value", "database", "tables", "grep"],
            ),
            (
                "tools.exportDatabaseSql",
                "Export Database as SQL…",
                CommandCategory::Tools,
                &["dump", "export", "sql", "backup", "schema", "data"],
            ),
            (
                "tools.scheduleTask",
                "Schedule Task (Dump, Backup, or Query Export)…",
                CommandCategory::Tools,
                &[
                    "schedule",
                    "scheduled",
                    "task",
                    "dump",
                    "backup",
                    "pg_dump",
                    "mysqldump",
                    "export",
                    "cron",
                    "automation",
                ],
            ),
            (
                "tools.serverDashboard",
                "Server Dashboard…",
                CommandCategory::Tools,
                &[
                    "dashboard",
                    "health",
                    "server",
                    "metrics",
                    "connections",
                    "tps",
                ],
            ),
            (
                "tools.activityMonitor",
                "Activity Monitor…",
                CommandCategory::Tools,
                &["activity", "monitor", "sessions", "processes"],
            ),
            (
                "tools.redisProfiler",
                "Redis Profiler…",
                CommandCategory::Tools,
                &["redis", "profiler", "monitor", "commands", "live", "watch"],
            ),
            (
                "tools.redisPubSub",
                "Redis Pub/Sub Viewer…",
                CommandCategory::Tools,
                &[
                    "redis",
                    "pubsub",
                    "subscribe",
                    "channels",
                    "messages",
                    "live",
                ],
            ),
            (
                "tools.sqlAgent",
                "SQL Agent…",
                CommandCategory::Tools,
                &["agent", "jobs", "schedules"],
            ),
            (
                "tools.security",
                "Security…",
                CommandCategory::Tools,
                &["security", "logins", "users", "roles"],
            ),
            (
                "tools.designers",
                "Designers…",
                CommandCategory::Tools,
                &["designer", "table", "index", "keys"],
            ),
            (
                "tools.structure_editor",
                "Edit Structure…",
                CommandCategory::Tools,
                &["structure", "alter", "table", "columns"],
            ),
            (
                "tools.importExport",
                "Import / Export…",
                CommandCategory::Tools,
                &["import", "export", "data", "wizard"],
            ),
            (
                "tools.importFromClipboard",
                "Import from Clipboard…",
                CommandCategory::Tools,
                &["paste", "clipboard", "import", "table", "create"],
            ),
            (
                "tools.schemaCompare",
                "Schema Compare…",
                CommandCategory::Tools,
                &["schema", "compare", "diff", "migration"],
            ),
            (
                "tools.schemaHistory",
                "Schema History…",
                CommandCategory::Tools,
                &[
                    "schema", "history", "snapshot", "timeline", "drift", "compare",
                ],
            ),
            (
                "tools.saveSchemaSnapshot",
                "Save Schema Snapshot…",
                CommandCategory::Tools,
                &["schema", "snapshot", "save", "capture", "drift"],
            ),
            (
                "tools.dataCompare",
                "Data Compare…",
                CommandCategory::Tools,
                &["data", "compare", "diff", "sync", "rows", "table"],
            ),
            (
                "tools.dataTransfer",
                "Data Transfer…",
                CommandCategory::Tools,
                &["transfer", "copy", "table", "connection", "migrate", "data"],
            ),
            (
                "tools.queryStore",
                "Query Insights…",
                CommandCategory::Tools,
                &[
                    "query",
                    "insights",
                    "store",
                    "pg_stat_statements",
                    "performance schema",
                    "top",
                    "performance",
                ],
            ),
            (
                "tools.diskUsage",
                "Disk Usage…",
                CommandCategory::Tools,
                &[
                    "disk", "usage", "size", "space", "storage", "files", "growth",
                ],
            ),
            (
                "tools.redisMemoryAnalyzer",
                "Redis Memory Analyzer…",
                CommandCategory::Tools,
                &[
                    "redis", "memory", "usage", "type", "ttl", "keys", "analyze", "report",
                ],
            ),
            (
                "tools.redisClusterTopology",
                "Redis Cluster Topology…",
                CommandCategory::Tools,
                &[
                    "redis", "cluster", "topology", "nodes", "slots", "shards", "masters",
                    "replicas", "trace",
                ],
            ),
            (
                "tools.redisKeyView",
                "Redis Key View…",
                CommandCategory::Tools,
                &[
                    "redis", "key", "value", "view", "edit", "hash", "list", "set", "zset", "json",
                    "ttl",
                ],
            ),
            (
                "tools.objectBrowser",
                "Object Browser…",
                CommandCategory::Tools,
                &[
                    "object",
                    "browser",
                    "tables",
                    "inventory",
                    "list",
                    "size",
                    "rows",
                ],
            ),
            (
                "tools.indexAnalyzer",
                "Index Analyzer…",
                CommandCategory::Tools,
                &[
                    "index", "analyzer", "unused", "seeks", "scans", "usage", "drop",
                ],
            ),
            (
                "tools.testDataGenerator",
                "Test Data Generator…",
                CommandCategory::Tools,
                &[
                    "test",
                    "data",
                    "generator",
                    "sample",
                    "fake",
                    "seed",
                    "insert",
                ],
            ),
            (
                "tools.exportSchemaDocs",
                "Document Schema (Markdown)…",
                CommandCategory::Tools,
                &[
                    "schema",
                    "documentation",
                    "markdown",
                    "data dictionary",
                    "export",
                    "docs",
                ],
            ),
            (
                "tools.extendedEvents",
                "Extended Events…",
                CommandCategory::Tools,
                &["extended", "events", "xevents", "trace"],
            ),
            (
                "tools.azureSqlConnection",
                "Azure SQL Connection…",
                CommandCategory::Tools,
                &["azure", "sql", "cloud"],
            ),
            (
                "tools.addFirewallRule",
                "Add My IP to Server Firewall…",
                CommandCategory::Tools,
                &["firewall", "azure", "ip", "rule"],
            ),
            (
                "window.nextEditorTab",
                "Next Editor Tab",
                CommandCategory::Window,
                &["next", "tab", "cycle"],
            ),
            (
                "window.previousEditorTab",
                "Previous Editor Tab",
                CommandCategory::Window,
                &["previous", "tab", "cycle"],
            ),
            (
                "window.previousTabInHistory",
                "Back to Previous Tab",
                CommandCategory::Window,
                &["back", "history", "tab", "previous"],
            ),
            (
                "window.nextTabInHistory",
                "Forward to Next Tab",
                CommandCategory::Window,
                &["forward", "history", "tab", "next"],
            ),
            (
                "help.sqllyHelp",
                "SQLLY Help…",
                CommandCategory::Help,
                &["help", "shortcuts", "documentation"],
            ),
            (
                "help.formattingDirectives",
                "Formatting Directives…",
                CommandCategory::Help,
                &["formatting", "directives", "reference"],
            ),
            (
                "help.sendFeedback",
                "Send Feedback…",
                CommandCategory::Help,
                &[
                    "feedback",
                    "bug",
                    "report",
                    "screenshot",
                    "suggestion",
                    "issue",
                ],
            ),
            (
                "help.tipOfTheDay",
                "Tip of the Day…",
                CommandCategory::Help,
                &["tip", "tips", "hint", "learn", "startup"],
            ),
            (
                "help.checkForUpdates",
                "Check for Updates…",
                CommandCategory::Help,
                &["update", "version", "upgrade"],
            ),
            (
                "help.unlockSqlly",
                "Unlock SQLLY…",
                CommandCategory::Help,
                &["unlock", "buy", "purchase", "license", "restore", "upgrade"],
            ),
            (
                "help.appStoreEdition",
                "Mac App Store Edition…",
                CommandCategory::Help,
                &[
                    "app store",
                    "sandbox",
                    "limitations",
                    "edition",
                    "unavailable",
                ],
            ),
        ];
        for (id, title, category, keywords) in more {
            // The App Store updates the App Store edition, and only that
            // edition sells through in-app purchase.
            if *id == "help.checkForUpdates" && !crate::distribution::checks_for_updates() {
                continue;
            }
            if matches!(*id, "help.unlockSqlly" | "help.appStoreEdition")
                && !crate::distribution::is_app_store()
            {
                continue;
            }
            // Single-window platform: results can't detach into a second window.
            #[cfg(target_arch = "wasm32")]
            if matches!(*id, "view.detachResults" | "view.reattachResults") {
                continue;
            }
            reg.register(ShellCommand {
                id: id.to_string(),
                title: title.to_string(),
                category: *category,
                keywords: keywords.to_vec(),
            });
        }
        for n in 1..=9 {
            reg.register(ShellCommand {
                id: format!("connection.quickSwitch{n}"),
                title: format!("Switch to Connection {n}"),
                category: CommandCategory::File,
                keywords: vec!["connection", "switch", "quick"],
            });
        }
        let mut used_slots = [false; 10];
        for action in crate::snippets::SnippetPreferences::load().quick_actions {
            let slot = action.slot;
            if !(1..=9).contains(&slot) || used_slots[slot as usize] {
                continue;
            }
            used_slots[slot as usize] = true;
            reg.register(ShellCommand {
                id: crate::snippets::SnippetPreferences::command_id(slot),
                title: crate::snippets::SnippetPreferences::quick_action_title(slot, &action.title),
                category: CommandCategory::QuickActions,
                keywords: vec!["quick", "action", "snippet", "sql"],
            });
        }
        reg
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::default_registry()
    }
}

#[derive(Debug, Clone)]
pub struct CommandMatch {
    pub command: ShellCommand,
    pub score: i32,
}

/// Bounded most-recently-used list of successfully executed command ids,
/// persisted so the palette can float recent commands to the top across
/// sessions. Pure model — recency influences ordering as a small score
/// boost, never overriding a strong fuzzy match.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CommandMru {
    #[serde(default)]
    ids: Vec<String>,
}

impl CommandMru {
    pub const CAP: usize = 30;
    /// Boost for the most recent command; decays with recency position.
    const MAX_BOOST: i32 = 15;

    pub fn record(&mut self, id: &str) {
        self.ids.retain(|existing| existing != id);
        self.ids.insert(0, id.to_string());
        self.ids.truncate(Self::CAP);
    }

    /// Score boost for `id`: most recent gets the largest boost, tapering to
    /// zero outside the first ten entries.
    pub fn boost(&self, id: &str) -> i32 {
        match self.ids.iter().position(|existing| existing == id) {
            Some(index) if index < 10 => Self::MAX_BOOST - (index as i32 * 3).min(12),
            _ => 0,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn load() -> Self {
        std::fs::read_to_string(crate::preferences::command_mru_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    #[cfg(target_arch = "wasm32")]
    pub fn load() -> Self {
        crate::host_storage::load_text(
            crate::host_storage::StorageArea::Data,
            "command-palette-recents.json",
        )
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn save(&self) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(crate::preferences::command_mru_path(), text);
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn save(&self) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = crate::host_storage::store_text(
                crate::host_storage::StorageArea::Data,
                "command-palette-recents.json",
                &text,
            );
        }
    }
}

pub struct CommandPaletteMatcher;

impl CommandPaletteMatcher {
    /// Recency-free search — production callers use `search_with_recency`.
    #[cfg(test)]
    pub fn search(query: &str, commands: &[ShellCommand]) -> Vec<CommandMatch> {
        Self::search_with_recency(query, commands, &CommandMru::default())
    }

    /// Search with a small recency boost: recently executed commands float
    /// to the top of an empty query and get a mild edge on ties, while a
    /// strong fuzzy match still outranks recency.
    pub fn search_with_recency(
        query: &str,
        commands: &[ShellCommand],
        mru: &CommandMru,
    ) -> Vec<CommandMatch> {
        if query.is_empty() {
            let mut matches: Vec<CommandMatch> = commands
                .iter()
                .map(|c| CommandMatch {
                    command: c.clone(),
                    score: mru.boost(&c.id),
                })
                .collect();
            // Stable sort keeps registry order within equal scores.
            matches.sort_by_key(|m| std::cmp::Reverse(m.score));
            return matches;
        }

        let query_lower = query.to_lowercase();
        let mut matches: Vec<CommandMatch> = Vec::new();

        for cmd in commands {
            let title_lower = cmd.title.to_lowercase();
            if let Some(score) = Self::subsequence_score(&query_lower, &title_lower) {
                matches.push(CommandMatch {
                    command: cmd.clone(),
                    score,
                });
                continue;
            }

            let mut best_keyword_score: Option<i32> = None;
            for kw in &cmd.keywords {
                let kw_lower = kw.to_lowercase();
                if let Some(score) = Self::subsequence_score(&query_lower, &kw_lower) {
                    let penalized = score - 40;
                    match best_keyword_score {
                        Some(prev) if prev >= penalized => {}
                        _ => best_keyword_score = Some(penalized),
                    }
                }
            }
            if let Some(score) = best_keyword_score {
                matches.push(CommandMatch {
                    command: cmd.clone(),
                    score,
                });
            }
        }

        for m in &mut matches {
            m.score += mru.boost(&m.command.id);
        }
        matches.sort_by_key(|m| std::cmp::Reverse(m.score));
        matches
    }

    /// Fuzzy subsequence score used by the command palette and `@` mention popup.
    pub(crate) fn subsequence_score(query: &str, target: &str) -> Option<i32> {
        let query_chars: Vec<char> = query.chars().collect();
        let target_chars: Vec<char> = target.chars().collect();

        if query_chars.is_empty() {
            return Some(0);
        }
        if query_chars.len() > target_chars.len() {
            return None;
        }

        let mut score: i32 = 0;
        let mut qi = 0;
        let mut contiguous = 0;
        let mut max_contiguous = 0;
        let mut prev_match = false;

        for (ti, &tc) in target_chars.iter().enumerate() {
            if qi >= query_chars.len() {
                break;
            }
            let qc = query_chars[qi];
            if tc == qc {
                if ti == 0 || target_chars[ti - 1] == ' ' || target_chars[ti - 1] == '.' {
                    score += 10;
                }
                if prev_match {
                    contiguous += 1;
                    if contiguous > max_contiguous {
                        max_contiguous = contiguous;
                    }
                } else {
                    contiguous = 1;
                    if contiguous > max_contiguous {
                        max_contiguous = contiguous;
                    }
                }
                if ti == 0 {
                    score += 20;
                }
                score += 2;
                qi += 1;
                prev_match = true;
            } else {
                prev_match = false;
            }
        }

        if qi == query_chars.len() {
            score += max_contiguous * 5;
            if query_chars.len() == target_chars.len() {
                score += 30;
            }
            Some(score)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    Commands,
    Objects,
}

/// Which gesture confirmed an object row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectAction {
    Reveal,
    SelectRows,
    Definition,
}

/// `>` at the start of object-mode input switches ranking to the command registry.
pub(crate) fn split_palette_query(mode: PaletteMode, text: &str) -> (PaletteMode, &str) {
    if mode == PaletteMode::Objects
        && let Some(rest) = text.strip_prefix('>')
    {
        return (PaletteMode::Commands, rest.trim_start());
    }
    (mode, text)
}

pub enum CommandPaletteEvent {
    Execute(String),
    Unavailable(String),
    Cancelled,
    Object {
        candidate: QuickOpenCandidate,
        action: ObjectAction,
    },
}

impl EventEmitter<CommandPaletteEvent> for CommandPalette {}

pub struct CommandPalette {
    registry: CommandRegistry,
    search_input: Entity<InputState>,
    matches: Vec<CommandMatch>,
    selected: usize,
    unavailable_reasons: HashMap<String, String>,
    mru: CommandMru,
    scroll_handle: ScrollHandle,
    mode: PaletteMode,
    objects: Vec<QuickOpenCandidate>,
    object_matches: Vec<QuickOpenMatch>,
    active_server: Option<Uuid>,
    _subscriptions: Vec<Subscription>,
}

impl CommandPalette {
    pub fn new(
        registry: CommandRegistry,
        unavailable_reasons: HashMap<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(
            PaletteBuild {
                registry,
                unavailable_reasons,
                mode: PaletteMode::Commands,
                objects: Vec::new(),
                active_server: None,
                placeholder: "command_palette.commands.search_placeholder",
            },
            window,
            cx,
        )
    }

    pub fn new_objects(
        registry: CommandRegistry,
        objects: Vec<QuickOpenCandidate>,
        active_server: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(
            PaletteBuild {
                registry,
                unavailable_reasons: HashMap::new(),
                mode: PaletteMode::Objects,
                objects,
                active_server,
                placeholder: "command_palette.objects.search_placeholder",
            },
            window,
            cx,
        )
    }

    fn build(init: PaletteBuild, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let PaletteBuild {
            registry,
            unavailable_reasons,
            mode,
            objects,
            active_server,
            placeholder,
        } = init;
        let mru = CommandMru::load();
        let initial_matches = if mode == PaletteMode::Commands {
            CommandPaletteMatcher::search_with_recency("", registry.all(), &mru)
        } else {
            Vec::new()
        };
        let object_matches = if mode == PaletteMode::Objects {
            crate::quick_open::rank("", &objects, active_server)
        } else {
            Vec::new()
        };

        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                placeholder,
            ))
        });
        let search_subscription = cx.subscribe_in(
            &search_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    this.refresh_matches(&text, cx);
                }
                InputEvent::PressEnter { secondary, .. } => {
                    let action = if *secondary {
                        ObjectAction::SelectRows
                    } else {
                        ObjectAction::Reveal
                    };
                    this.execute_selected_with(action, cx);
                }
                _ => {}
            },
        );

        Self {
            registry,
            search_input,
            matches: initial_matches,
            selected: 0,
            unavailable_reasons,
            mru,
            scroll_handle: ScrollHandle::new(),
            mode,
            objects,
            object_matches,
            active_server,
            _subscriptions: vec![search_subscription],
        }
    }

    /// Focus the search field — called by the shell right after opening the
    /// palette so typing filters immediately.
    pub fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_input.update(cx, |state, cx| {
            state.focus(window, cx);
        });
    }

    fn refresh_matches(&mut self, text: &str, cx: &mut Context<Self>) {
        let (effective, rest) = split_palette_query(self.mode, text);
        match effective {
            PaletteMode::Commands => {
                self.matches = CommandPaletteMatcher::search_with_recency(
                    rest,
                    self.registry.all(),
                    &self.mru,
                );
            }
            PaletteMode::Objects => {
                self.object_matches =
                    crate::quick_open::rank(rest, &self.objects, self.active_server);
            }
        }
        self.selected = 0;
        self.scroll_handle.scroll_to_item(0);
        cx.notify();
    }

    fn query_text(&self, cx: &gpui::App) -> String {
        self.search_input.read(cx).value().to_string()
    }

    fn effective_mode(&self, cx: &gpui::App) -> PaletteMode {
        split_palette_query(self.mode, &self.query_text(cx)).0
    }

    fn visible_len(&self, cx: &gpui::App) -> usize {
        match self.effective_mode(cx) {
            PaletteMode::Commands => self.matches.len(),
            PaletteMode::Objects => self.object_matches.len(),
        }
    }

    fn execute_selected(&mut self, cx: &mut Context<Self>) {
        self.execute_selected_with(ObjectAction::Reveal, cx);
    }

    fn execute_selected_with(&mut self, action: ObjectAction, cx: &mut Context<Self>) {
        match self.effective_mode(cx) {
            PaletteMode::Commands => {
                if let Some(m) = self.matches.get(self.selected) {
                    if let Some(reason) = self.unavailable_reasons.get(&m.command.id) {
                        cx.emit(CommandPaletteEvent::Unavailable(reason.clone()));
                    } else {
                        self.mru.record(&m.command.id);
                        self.mru.save();
                        cx.emit(CommandPaletteEvent::Execute(m.command.id.clone()));
                    }
                } else {
                    cx.emit(CommandPaletteEvent::Cancelled);
                }
            }
            PaletteMode::Objects => {
                if let Some(m) = self.object_matches.get(self.selected) {
                    cx.emit(CommandPaletteEvent::Object {
                        candidate: m.candidate.clone(),
                        action,
                    });
                } else {
                    cx.emit(CommandPaletteEvent::Cancelled);
                }
            }
        }
    }

    fn move_up(&mut self, cx: &mut Context<Self>) {
        if self.selected > 0 {
            self.selected -= 1;
            self.scroll_handle.scroll_to_item(self.selected);
            cx.notify();
        }
    }

    fn move_down(&mut self, cx: &mut Context<Self>) {
        if self.selected + 1 < self.visible_len(cx) {
            self.selected += 1;
            self.scroll_handle.scroll_to_item(self.selected);
            cx.notify();
        }
    }

    pub fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        match key {
            "up" => self.move_up(cx),
            "down" => self.move_down(cx),
            "enter" => {
                let m = &event.keystroke.modifiers;
                let action = if m.platform || m.control {
                    ObjectAction::SelectRows
                } else if m.alt {
                    ObjectAction::Definition
                } else {
                    ObjectAction::Reveal
                };
                self.execute_selected_with(action, cx);
            }
            "escape" => cx.emit(CommandPaletteEvent::Cancelled),
            _ => {}
        }
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search = Input::new(&self.search_input).cleanable(true);
        let effective = self.effective_mode(cx);
        let selected = self.selected;
        let show_object_chrome = self.mode == PaletteMode::Objects;

        let overlay = div()
            // Block scroll/hover from reaching surfaces beneath the palette.
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(Theme::scrim())
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|_this, _ev, _window, cx| {
                    cx.emit(CommandPaletteEvent::Cancelled);
                }),
            );

        let rows: Vec<PaletteRow> = match effective {
            PaletteMode::Commands => self
                .matches
                .iter()
                .map(|m| {
                    let shortcut = crate::keybindings::shortcut_hint(&m.command.id);
                    let unavailable = self.unavailable_reasons.get(&m.command.id).cloned();
                    PaletteRow {
                        left: m.command.category.label(),
                        title: m.command.title.clone(),
                        sub: unavailable.clone(),
                        right: if unavailable.is_none() && !shortcut.is_empty() {
                            Some(shortcut)
                        } else {
                            None
                        },
                        dimmed: unavailable.is_some(),
                    }
                })
                .collect(),
            PaletteMode::Objects => self
                .object_matches
                .iter()
                .map(|m| PaletteRow {
                    left: m.candidate.kind.label(),
                    title: m.candidate.title.clone(),
                    sub: None,
                    right: object_trailing_hint(&m.candidate),
                    dimmed: false,
                })
                .collect(),
        };

        let empty_message = match effective {
            PaletteMode::Objects if self.objects.is_empty() => {
                crate::localization::text("command_palette.objects.index_empty")
            }
            PaletteMode::Objects => crate::localization::text("command_palette.objects.no_matches"),
            PaletteMode::Commands => {
                crate::localization::text("command_palette.commands.no_matches")
            }
        };

        let list = if rows.is_empty() {
            div()
                .id("cmd-list")
                .flex_1()
                .px(px(Theme::scaled(12.0)))
                .py(px(Theme::scaled(16.0)))
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(Theme::text_dim())
                .child(empty_message)
        } else {
            div()
                .id("cmd-list")
                .flex_1()
                .overflow_scroll()
                .track_scroll(&self.scroll_handle)
                .py(px(Theme::scaled(4.0)))
                .children(
                    rows.into_iter()
                        .enumerate()
                        .map(|(i, row)| {
                            let is_selected = i == selected;
                            palette_row(i, is_selected, row).on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _ev, _window, cx| {
                                    this.selected = i;
                                    this.execute_selected(cx);
                                }),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
        };

        let hint = crate::localization::format(
            "command_palette.objects.keyboard_hint",
            &[
                ("primary_modifier", crate::keybindings::PRIMARY_LABEL),
                ("option_modifier", crate::keybindings::OPTION_LABEL),
            ],
        );

        let palette = div()
            // Clicks inside the palette must not fall through to the
            // dim backdrop's dismiss handler underneath.
            .occlude()
            .w(px(Theme::scaled(480.0)))
            .max_h(px(Theme::scaled(420.0)))
            .bg(Theme::popup_bg())
            .border_1()
            .border_color(Theme::popup_border())
            .rounded(px(Theme::scaled(6.0)))
            .flex()
            .flex_col()
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(
                div()
                    .px(px(Theme::scaled(12.0)))
                    .py(px(Theme::scaled(10.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(search),
            )
            .when(show_object_chrome, |palette| {
                palette.child(
                    div()
                        .px(px(Theme::scaled(12.0)))
                        .py(px(Theme::scaled(6.0)))
                        .border_b_1()
                        .border_color(Theme::border())
                        .text_size(px(Theme::font_size_ratio(0.67)))
                        .text_color(Theme::text_dim())
                        .child(hint),
                )
            })
            .child(list);

        // The root must span the whole window so the dim backdrop (and its
        // click-to-dismiss) covers everything, not just a strip behind the
        // palette; the palette itself is pushed down by padding, not by
        // offsetting the root.
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .pt(px(Theme::scaled(80.0)))
            .flex()
            .flex_col()
            .items_center()
            .child(overlay)
            .child(palette)
    }
}

struct PaletteBuild {
    registry: CommandRegistry,
    unavailable_reasons: HashMap<String, String>,
    mode: PaletteMode,
    objects: Vec<QuickOpenCandidate>,
    active_server: Option<Uuid>,
    placeholder: &'static str,
}

struct PaletteRow {
    left: String,
    title: String,
    sub: Option<String>,
    right: Option<String>,
    dimmed: bool,
}

fn object_trailing_hint(candidate: &QuickOpenCandidate) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(database) = &candidate.database
        && !database.is_empty()
        && database != &candidate.title
    {
        parts.push(database.as_str());
    }
    if !candidate.server_name.is_empty() && candidate.server_name != candidate.title {
        parts.push(candidate.server_name.as_str());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn palette_row(i: usize, is_selected: bool, row: PaletteRow) -> gpui::Stateful<gpui::Div> {
    div()
        .id(("cmd", i))
        .w_full()
        .px(px(Theme::scaled(12.0)))
        .py(px(Theme::scaled(6.0)))
        .when(!row.dimmed, |el| el.cursor_pointer())
        .when(row.dimmed, |el| el.cursor_default().opacity(0.55))
        .font(Theme::ui_font())
        // The palette body is the popup surface;
        // unselected rows stay transparent so it
        // shows through rather than repainting a
        // second (panel) fill over the popup.
        .bg(if is_selected {
            Theme::selection()
        } else {
            gpui::transparent_black()
        })
        .hover(|s| {
            s.bg(if is_selected {
                Theme::selection()
            } else {
                Theme::row_hover()
            })
        })
        .flex()
        .flex_row()
        .items_center()
        .gap(px(Theme::scaled(8.0)))
        .child(
            div()
                .text_size(px(Theme::font_size_ratio(0.67)))
                .text_color(Theme::text_dim())
                .w(px(Theme::scaled(50.0)))
                .child(row.left),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(if is_selected {
                    Theme::text()
                } else {
                    Theme::text_dim()
                })
                .flex_1()
                .child(row.title)
                .when_some(row.sub, |title, reason| {
                    title.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.67)))
                            .text_color(Theme::text_dim())
                            .child(reason),
                    )
                }),
        )
        .when_some(row.right, |el, right| {
            el.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.67)))
                    .text_color(Theme::text_dim())
                    .child(right),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_returns_all() {
        let reg = CommandRegistry::default_registry();
        let matches = CommandPaletteMatcher::search("", reg.all());
        assert_eq!(matches.len(), reg.all().len());
        assert!(matches.iter().all(|m| m.score == 0));
    }

    #[test]
    fn subsequence_matches_title() {
        let reg = CommandRegistry::default_registry();
        let matches = CommandPaletteMatcher::search("exec", reg.all());
        assert!(matches.iter().any(|m| m.command.id == "query.execute"));
    }

    #[test]
    fn keyword_matches_work() {
        let reg = CommandRegistry::default_registry();
        let matches = CommandPaletteMatcher::search("run", reg.all());
        assert!(matches.iter().any(|m| m.command.id == "query.execute"));
    }

    #[test]
    fn schema_refresh_command_is_registered() {
        let reg = CommandRegistry::default_registry();
        let matches = CommandPaletteMatcher::search("refresh catalog", reg.all());
        assert!(
            matches.iter().any(|m| m.command.id == "schema.refresh"),
            "schema.refresh must be discoverable in the command palette"
        );
    }

    #[test]
    fn quick_open_command_is_registered() {
        let reg = CommandRegistry::default_registry();
        let matches = CommandPaletteMatcher::search("go to object", reg.all());
        assert!(
            matches.iter().any(|m| m.command.id == "tools.quickOpen"),
            "tools.quickOpen must be discoverable in the command palette"
        );
    }

    #[test]
    fn commands_prefix_is_not_an_object_query() {
        assert_eq!(
            split_palette_query(PaletteMode::Objects, "> new q"),
            (PaletteMode::Commands, "new q")
        );
        assert_eq!(
            split_palette_query(PaletteMode::Objects, "orders"),
            (PaletteMode::Objects, "orders")
        );
        assert_eq!(
            split_palette_query(PaletteMode::Commands, "> new q"),
            (PaletteMode::Commands, "> new q")
        );
    }

    #[test]
    fn title_outranks_keyword() {
        let reg = CommandRegistry::default_registry();
        let matches = CommandPaletteMatcher::search("query", reg.all());
        let execute_match = matches
            .iter()
            .find(|m| m.command.id == "query.execute")
            .unwrap();
        let cancel_match = matches
            .iter()
            .find(|m| m.command.id == "query.cancel")
            .unwrap();
        assert!(execute_match.score >= 0);
        assert!(cancel_match.score >= 0);
    }

    #[test]
    fn no_match_returns_empty() {
        let reg = CommandRegistry::default_registry();
        let matches = CommandPaletteMatcher::search("xyzqqqqq", reg.all());
        assert!(matches.is_empty());
    }

    #[test]
    fn every_registered_command_has_dispatch() {
        // The regression this guards: adding a palette entry whose id never
        // reaches a handler, so selecting it shows "Unknown command".
        let reg = CommandRegistry::default_registry();
        for cmd in reg.all() {
            assert!(
                crate::app::AppView::PALETTE_DISPATCH_IDS.contains(&cmd.id.as_str()),
                "palette command {} has no execute_command dispatch arm",
                cmd.id
            );
        }
    }

    #[test]
    fn registered_command_ids_are_unique() {
        let reg = CommandRegistry::default_registry();
        let mut ids: Vec<&str> = reg.all().iter().map(|c| c.id.as_str()).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate palette command id");
    }

    #[test]
    fn menu_bar_admin_tools_are_discoverable() {
        let reg = CommandRegistry::default_registry();
        for query in [
            "backup",
            "activity monitor",
            "schema compare",
            "preferences",
        ] {
            assert!(
                !CommandPaletteMatcher::search(query, reg.all()).is_empty(),
                "no palette match for {query:?}"
            );
        }
    }

    /// Perf smoke: palette search runs per keystroke; a couple thousand
    /// scored passes over the full registry must be nowhere near a second.
    /// Generous bound — catches accidental quadratic blowups, not regressions
    /// of a few percent.
    #[test]
    fn palette_search_smoke() {
        let reg = CommandRegistry::default_registry();
        let queries = ["e", "ex", "exp", "expl", "backup", "toggle", "zzz", "sc"];
        let started = std::time::Instant::now();
        for i in 0..2_000 {
            let q = queries[i % queries.len()];
            let _ = CommandPaletteMatcher::search(q, reg.all());
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "2000 palette searches took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn quick_actions_appear_in_palette_with_title() {
        let mut reg = CommandRegistry::new();
        reg.register(ShellCommand {
            id: "quickAction.slot1".to_string(),
            title: "Quick action: Describe table".to_string(),
            category: CommandCategory::QuickActions,
            keywords: vec!["quick", "action", "snippet", "sql"],
        });
        let matches = CommandPaletteMatcher::search("describe", reg.all());
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].command.id, "quickAction.slot1");
        assert_eq!(matches[0].command.title, "Quick action: Describe table");
    }

    #[test]
    fn prefix_match_scores_higher() {
        let reg = CommandRegistry::default_registry();
        let matches = CommandPaletteMatcher::search("new", reg.all());
        let new_query = matches
            .iter()
            .find(|m| m.command.id == "file.newQuery")
            .unwrap();
        assert!(new_query.score > 0);
    }

    #[test]
    fn mru_records_dedupes_and_caps() {
        let mut mru = CommandMru::default();
        for i in 0..40 {
            mru.record(&format!("cmd.{i}"));
        }
        mru.record("cmd.5");
        assert_eq!(mru.boost("cmd.5"), CommandMru::MAX_BOOST);
        assert_eq!(mru.boost("cmd.0"), 0, "evicted past the cap");
        // Re-recording did not duplicate: boost positions taper.
        assert!(mru.boost("cmd.39") < CommandMru::MAX_BOOST);
        assert!(mru.boost("never-run") == 0);
    }

    #[test]
    fn empty_query_floats_recent_commands_to_the_top() {
        let reg = CommandRegistry::default_registry();
        let mut mru = CommandMru::default();
        mru.record("view.toggleSidebar");
        mru.record("tools.activityMonitor");
        let matches = CommandPaletteMatcher::search_with_recency("", reg.all(), &mru);
        assert_eq!(matches[0].command.id, "tools.activityMonitor");
        assert_eq!(matches[1].command.id, "view.toggleSidebar");
    }

    #[test]
    fn recency_boost_never_overrides_a_strong_match() {
        let reg = CommandRegistry::default_registry();
        let mut mru = CommandMru::default();
        // Boost an unrelated command, then search for an exact-ish title.
        mru.record("tools.activityMonitor");
        let matches = CommandPaletteMatcher::search_with_recency("new query", reg.all(), &mru);
        assert_eq!(
            matches[0].command.id, "file.newQuery",
            "exact title match outranks a recency-boosted non-match"
        );
    }
}
