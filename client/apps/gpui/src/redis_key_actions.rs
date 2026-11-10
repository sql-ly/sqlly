//! The Redis key browser's move-and-copy dialogs: Rename… (`RENAMENX`, or
//! `RENAME` when overwriting is chosen), Copy to database… (`COPY key key
//! DB n [REPLACE]`), Copy to connection… (`DUMP` + `PTTL` on the key's own
//! connection, then `RESTORE … [REPLACE]` on the chosen one), Export as
//! command file… (`SQLLY.DUMPSCRIPT`, written to a local `.redis` file the
//! bulk import runs back byte-exact), and the database node's Rename…,
//! which names a logical database (`db3 — sessions`) on this profile.
//!
//! Every write shows the exact command before it runs and goes through
//! `tool_dialog::run_query`, which skips the editor's run gate — so these
//! dialogs carry its checks themselves, as the other key dialogs do: a
//! read-only or locked production connection disables the write (the
//! production guard is re-read right before it), and a connection whose
//! safety settings would make a query tab confirm asks for a second click.
//! Copy to connection checks the *target* connection the same way before
//! anything is restored there. The export only reads.

use std::collections::BTreeMap;
use std::rc::Rc;

use gpui::{
    Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render, Styled,
    Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_core::{CellValue, ConnectionProfile};
use sqlly_intel::query::safety::ProductionDecision;
use sqlly_intel::query::safety::redis::quote_argument;

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ProductionCheck, ToolDialogContext, ToolQueryOutcome};

/// Keys an export walks unless the user asks for another limit.
pub const EXPORT_DEFAULT_LIMIT: usize = 10_000;
/// The most an export may ask for (the import caps files at 8 MB anyway).
pub const EXPORT_MAX_LIMIT: usize = 100_000;

/// Builds the routing and the production verdict for a target connection
/// (Copy to connection). Supplied by `AppView`, which owns both.
pub type TargetResolver =
    Rc<dyn Fn(&ConnectionProfile, &gpui::App) -> (ToolDialogContext, ProductionDecision)>;

/// What the dialog is for.
#[derive(Clone)]
pub enum RedisKeyActionMode {
    Rename {
        node_id: String,
        key: String,
    },
    CopyToDatabase {
        node_id: String,
        key: String,
        /// The key's own database, which a copy cannot target.
        current_db: i64,
        /// The profile's database names, for the label under the field.
        aliases: BTreeMap<i64, String>,
    },
    CopyToConnection {
        node_id: String,
        key: String,
        /// The Redis connections the key may be copied to (this one too:
        /// another database on the same server is a valid target).
        targets: Vec<ConnectionProfile>,
        resolver: TargetResolver,
    },
    ExportFolder {
        node_id: String,
        pattern: String,
    },
    /// Name logical database `db` on the profile (blank clears the name).
    NameDatabase {
        node_id: String,
        server_id: uuid::Uuid,
        db: i64,
        current: Option<String>,
    },
    /// Which logical databases the explorer lists for the profile.
    VisibleDatabases {
        node_id: String,
        server_id: uuid::Uuid,
        current: Option<Vec<i64>>,
    },
}

impl RedisKeyActionMode {
    fn node_id(&self) -> &str {
        match self {
            Self::Rename { node_id, .. }
            | Self::CopyToDatabase { node_id, .. }
            | Self::CopyToConnection { node_id, .. }
            | Self::ExportFolder { node_id, .. }
            | Self::NameDatabase { node_id, .. }
            | Self::VisibleDatabases { node_id, .. } => node_id,
        }
    }

    fn writes(&self) -> bool {
        matches!(
            self,
            Self::Rename { .. } | Self::CopyToDatabase { .. } | Self::CopyToConnection { .. }
        )
    }
}

/// What the dialog asks of the app.
#[derive(Clone, Debug)]
pub enum RedisKeyActionEvent {
    /// A command ran (or a file was written): reload the node, say so.
    Changed {
        node_id: String,
        message: String,
    },
    /// Store this name for logical database `db` of the profile.
    NameDatabase {
        node_id: String,
        server_id: uuid::Uuid,
        db: i64,
        alias: Option<String>,
    },
    /// Store the profile's visible-databases list (`None` lists them all).
    VisibleDatabases {
        node_id: String,
        server_id: uuid::Uuid,
        visible: Option<Vec<i64>>,
    },
    Close,
}

/// `RENAMENX key new` (refuses an existing target) or `RENAME key new`.
pub fn rename_command(key: &str, new_name: &str, only_if_free: bool) -> String {
    format!(
        "{} {} {}",
        if only_if_free { "RENAMENX" } else { "RENAME" },
        quote_argument(key.as_bytes()),
        quote_argument(new_name.as_bytes())
    )
}

/// `COPY key key DB n [REPLACE]`.
pub fn copy_to_database_command(key: &str, db: i64, replace: bool) -> String {
    let key = quote_argument(key.as_bytes());
    let mut text = format!("COPY {key} {key} DB {db}");
    if replace {
        text.push_str(" REPLACE");
    }
    text
}

/// The source half of Copy to connection: the serialized value, then its
/// remaining time to live in milliseconds.
pub fn dump_script(key: &str) -> String {
    let key = quote_argument(key.as_bytes());
    format!("DUMP {key}\nPTTL {key}")
}

/// The target half: `RESTORE key ttl payload [REPLACE]`, the payload
/// byte-exact as escapes. A key with no expiry restores with ttl 0.
pub fn restore_command(key: &str, pttl_ms: i64, payload: &[u8], replace: bool) -> String {
    let mut text = format!(
        "RESTORE {} {} {}",
        quote_argument(key.as_bytes()),
        pttl_ms.max(0),
        quote_argument(payload)
    );
    if replace {
        text.push_str(" REPLACE");
    }
    text
}

/// How the review shows the restore before the payload is known.
pub fn restore_preview(key: &str, replace: bool) -> String {
    format!(
        "RESTORE {} <ttl> <serialized value>{}",
        quote_argument(key.as_bytes()),
        if replace { " REPLACE" } else { "" }
    )
}

/// `SQLLY.DUMPSCRIPT pattern limit`.
pub fn export_command(pattern: &str, limit: usize) -> String {
    format!(
        "SQLLY.DUMPSCRIPT {} {}",
        quote_argument(pattern.as_bytes()),
        limit.clamp(1, EXPORT_MAX_LIMIT)
    )
}

/// The export's rows as the file's text (one line each, newline-ended).
pub fn export_text(rows: &[Vec<CellValue>]) -> String {
    let mut text = String::new();
    for row in rows {
        if let Some(CellValue::String(line)) = row.first() {
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

/// The export's closing comment, for the status bar (`complete: …` or
/// `partial: …`).
pub fn export_summary(text: &str) -> String {
    text.lines()
        .rev()
        .find_map(|l| l.strip_prefix("# "))
        .map(str::to_string)
        .unwrap_or_else(|| {
            crate::localization::text("explorer.redis_key_actions.exported_fallback")
        })
}

/// A logical database number typed into a field: digits only, not the
/// key's own database.
pub fn parse_target_db(text: &str, current: Option<i64>) -> Result<i64, String> {
    let n = text
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|n| *n >= 0)
        .ok_or_else(|| crate::localization::text("explorer.redis_key_actions.invalid_database"))?;
    if Some(n) == current {
        return Err(crate::localization::text(
            "explorer.redis_key_actions.same_database",
        ));
    }
    Ok(n)
}

/// Read the source half's two replies: the payload (`None` when the key
/// is gone) and the TTL in milliseconds (0 for no expiry).
pub fn parse_dump(outcome: &ToolQueryOutcome) -> Result<(Vec<u8>, i64), String> {
    let cell = |set: usize| {
        outcome
            .sets
            .get(set)
            .and_then(|s| s.rows.first())
            .and_then(|r| r.first())
            .cloned()
    };
    let payload = match cell(0) {
        Some(CellValue::Bytes(b)) => b,
        Some(CellValue::String(s)) => s.into_bytes(),
        _ => {
            return Err(crate::localization::text(
                "explorer.redis_key_actions.key_missing",
            ));
        }
    };
    let pttl = match cell(1) {
        Some(CellValue::I64(ms)) if ms > 0 => ms,
        _ => 0,
    };
    Ok((payload, pttl))
}

/// Why a write must not run on `profile` (read-only policy, locked
/// production), or `None`.
fn write_block(profile: &ConnectionProfile, production: ProductionDecision) -> Option<String> {
    crate::redis_bulk::write_block_reason(
        profile.enforce_read_only || profile.block_all_but_select,
        production,
    )
}

fn target_label(profile: &ConnectionProfile) -> String {
    let db = profile
        .database
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or("0");
    format!(
        "{} — {}:{} db {db}",
        profile.name, profile.server, profile.port
    )
}

pub struct RedisKeyActionDialog {
    context: ToolDialogContext,
    mode: RedisKeyActionMode,
    input: Entity<InputState>,
    second_input: Entity<InputState>,
    typed: String,
    typed_second: String,
    /// Rename: RENAMENX. Copy: add REPLACE (inverted meaning, own flag).
    only_if_free: bool,
    replace: bool,
    target_select: Option<Entity<SelectState<Vec<String>>>>,
    target_index: Option<usize>,
    production: ProductionDecision,
    production_check: ProductionCheck,
    write_block: Option<String>,
    /// The connection's safety settings asked for a confirming click.
    armed: bool,
    running: bool,
    error: Option<String>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RedisKeyActionEvent> for RedisKeyActionDialog {}

impl RedisKeyActionDialog {
    pub fn new(
        context: ToolDialogContext,
        mode: RedisKeyActionMode,
        production: ProductionDecision,
        production_check: ProductionCheck,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let (placeholder, initial, second_placeholder, second_initial) = match &mode {
            RedisKeyActionMode::Rename { key, .. } => (
                crate::localization::text("explorer.redis_key_actions.rename_placeholder"),
                key.clone(),
                String::new(),
                String::new(),
            ),
            RedisKeyActionMode::CopyToDatabase { .. } => (
                crate::localization::text("explorer.redis_key_actions.target_database_placeholder"),
                String::new(),
                String::new(),
                String::new(),
            ),
            RedisKeyActionMode::CopyToConnection { .. } => (
                crate::localization::text(
                    "explorer.redis_key_actions.target_connection_database_placeholder",
                ),
                String::new(),
                String::new(),
                String::new(),
            ),
            RedisKeyActionMode::ExportFolder { .. } => (
                crate::localization::text("explorer.redis_key_actions.export_path_placeholder"),
                String::new(),
                crate::localization::text("explorer.redis_key_actions.export_limit_placeholder"),
                EXPORT_DEFAULT_LIMIT.to_string(),
            ),
            RedisKeyActionMode::NameDatabase { current, .. } => (
                crate::localization::text("explorer.redis_key_actions.database_name_placeholder"),
                current.clone().unwrap_or_default(),
                String::new(),
                String::new(),
            ),
            RedisKeyActionMode::VisibleDatabases { current, .. } => (
                crate::localization::text(
                    "explorer.redis_key_actions.visible_databases_placeholder",
                ),
                crate::schema_tree::redis::visible_databases_text(current.as_deref()),
                String::new(),
                String::new(),
            ),
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(initial.clone())
        });
        let second_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(second_placeholder)
                .default_value(second_initial.clone())
        });
        let mut subscriptions = vec![
            cx.subscribe_in(
                &input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| match event {
                    InputEvent::Change => {
                        this.typed = state.read(cx).value().to_string();
                        this.error = None;
                        this.armed = false;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => this.confirm(cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &second_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.typed_second = state.read(cx).value().to_string();
                        this.error = None;
                        cx.notify();
                    }
                },
            ),
        ];
        let mut target_select = None;
        if let RedisKeyActionMode::CopyToConnection { targets, .. } = &mode {
            let labels: Vec<String> = targets.iter().map(target_label).collect();
            let select = cx.new(|cx| SelectState::new(labels.clone(), None, window, cx));
            subscriptions.push(cx.subscribe_in(
                &select,
                window,
                move |this: &mut Self, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    if let SelectEvent::Confirm(Some(label)) = event {
                        this.target_index = labels.iter().position(|l| l == label);
                        this.error = None;
                        this.armed = false;
                        cx.notify();
                    }
                },
            ));
            target_select = Some(select);
        }
        let write_block = mode
            .writes()
            .then(|| write_block(&context.profile, production))
            .flatten();
        Self {
            context,
            mode,
            input,
            second_input,
            typed: initial,
            typed_second: second_initial,
            only_if_free: true,
            replace: false,
            target_select,
            target_index: None,
            production,
            production_check,
            write_block,
            armed: false,
            running: false,
            error: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(Theme::scaled(520.0)),
            _subscriptions: subscriptions,
        }
    }

    /// The command the review shows (and, for the single-connection
    /// writes, runs), or `None` while the form is incomplete.
    pub fn command(&self) -> Option<String> {
        match &self.mode {
            RedisKeyActionMode::Rename { key, .. } => {
                let new_name = self.typed.trim();
                (!new_name.is_empty() && new_name != key)
                    .then(|| rename_command(key, new_name, self.only_if_free))
            }
            RedisKeyActionMode::CopyToDatabase {
                key, current_db, ..
            } => parse_target_db(&self.typed, Some(*current_db))
                .ok()
                .map(|db| copy_to_database_command(key, db, self.replace)),
            RedisKeyActionMode::CopyToConnection { key, .. } => self.target_index.map(|_| {
                format!(
                    "{}\n— then on the target —\n{}",
                    dump_script(key),
                    restore_preview(key, self.replace)
                )
            }),
            RedisKeyActionMode::ExportFolder { pattern, .. } => {
                Some(export_command(pattern, self.export_limit()))
            }
            RedisKeyActionMode::NameDatabase { .. }
            | RedisKeyActionMode::VisibleDatabases { .. } => None,
        }
    }

    fn export_limit(&self) -> usize {
        self.typed_second
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(EXPORT_DEFAULT_LIMIT)
            .min(EXPORT_MAX_LIMIT)
    }

    fn confirm_allowed(&self) -> bool {
        if self.running {
            return false;
        }
        if self.mode.writes() && self.write_block.is_some() {
            return false;
        }
        match &self.mode {
            RedisKeyActionMode::ExportFolder { .. } => !self.typed.trim().is_empty(),
            RedisKeyActionMode::NameDatabase { .. } => true,
            RedisKeyActionMode::VisibleDatabases { .. } => {
                crate::schema_tree::redis::parse_visible_databases(&self.typed).is_ok()
            }
            _ => self.command().is_some(),
        }
    }

    fn confirm(&mut self, cx: &mut gpui::Context<Self>) {
        if self.mode.writes() {
            self.production = (self.production_check)(cx);
            self.write_block = write_block(&self.context.profile, self.production);
        }
        if !self.confirm_allowed() {
            cx.notify();
            return;
        }
        match self.mode.clone() {
            RedisKeyActionMode::NameDatabase {
                node_id,
                server_id,
                db,
                ..
            } => {
                let alias = Some(self.typed.trim().to_string()).filter(|a| !a.is_empty());
                cx.emit(RedisKeyActionEvent::NameDatabase {
                    node_id,
                    server_id,
                    db,
                    alias,
                });
                cx.emit(RedisKeyActionEvent::Close);
            }
            RedisKeyActionMode::VisibleDatabases {
                node_id, server_id, ..
            } => {
                let Ok(visible) = crate::schema_tree::redis::parse_visible_databases(&self.typed)
                else {
                    return;
                };
                cx.emit(RedisKeyActionEvent::VisibleDatabases {
                    node_id,
                    server_id,
                    visible,
                });
                cx.emit(RedisKeyActionEvent::Close);
            }
            RedisKeyActionMode::ExportFolder { .. } => self.run_export(cx),
            RedisKeyActionMode::CopyToConnection {
                key,
                targets,
                resolver,
                ..
            } => self.run_copy_to_connection(key, targets, resolver, cx),
            RedisKeyActionMode::Rename { .. } | RedisKeyActionMode::CopyToDatabase { .. } => {
                let Some(command) = self.command() else {
                    return;
                };
                if !self.armed && self.would_confirm(&self.context.profile, &command) {
                    self.armed = true;
                    cx.notify();
                    return;
                }
                self.armed = false;
                self.run_single(command, cx);
            }
        }
    }

    fn would_confirm(&self, profile: &ConnectionProfile, text: &str) -> bool {
        let features = crate::mutation_review::SafetyFeatures::effective_for_profile(
            profile,
            crate::connection::ConnectionStore::global(),
        );
        crate::redis_key_view::confirmation_required(&features, self.production, text)
    }

    fn run_single(&mut self, command: String, cx: &mut gpui::Context<Self>) {
        let node_id = self.mode.node_id().to_string();
        self.running = true;
        self.error = None;
        cx.notify();
        let describe = command.clone();
        crate::tool_dialog::run_query(&self.context, command, cx, move |this, outcome, cx| {
            this.running = false;
            if let Some(error) = outcome.error {
                this.error = Some(error);
                cx.notify();
                return;
            }
            let reply = outcome
                .sets
                .first()
                .and_then(|s| s.rows.first())
                .and_then(|r| r.first())
                .cloned();
            // RENAMENX / COPY answer 0 when the target already exists.
            if matches!(reply, Some(CellValue::I64(0))) {
                this.error = Some(crate::localization::text(
                    "explorer.redis_key_actions.target_exists_no_change",
                ));
                cx.notify();
                return;
            }
            cx.emit(RedisKeyActionEvent::Changed {
                node_id,
                message: crate::localization::format(
                    "explorer.redis_key_actions.command_done",
                    &[("command", &describe)],
                ),
            });
            cx.emit(RedisKeyActionEvent::Close);
        });
    }

    fn run_copy_to_connection(
        &mut self,
        key: String,
        targets: Vec<ConnectionProfile>,
        resolver: TargetResolver,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(target) = self.target_index.and_then(|i| targets.get(i)).cloned() else {
            return;
        };
        let mut target = target;
        if !self.typed.trim().is_empty() {
            match parse_target_db(&self.typed, None) {
                Ok(db) => target.database = Some(db.to_string()),
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                    return;
                }
            }
        }
        if target.id == self.context.profile.id && target.database == self.context.profile.database
        {
            self.error = Some(crate::localization::text(
                "explorer.redis_key_actions.same_connection_database",
            ));
            cx.notify();
            return;
        }
        let (target_context, target_production) = resolver(&target, cx);
        if let Some(reason) = write_block(&target.clone(), target_production) {
            self.error = Some(crate::localization::format(
                "explorer.redis_key_actions.target_refuses_writes",
                &[("reason", &reason)],
            ));
            cx.notify();
            return;
        }
        let preview = restore_preview(&key, self.replace);
        if !self.armed && self.would_confirm(&target, &preview) {
            self.armed = true;
            cx.notify();
            return;
        }
        self.armed = false;
        self.running = true;
        self.error = None;
        cx.notify();
        let node_id = self.mode.node_id().to_string();
        let replace = self.replace;
        let target_name = target_label(&target);
        crate::tool_dialog::run_query(&self.context, dump_script(&key), cx, {
            move |this, outcome, cx| {
                if let Some(error) = outcome.error {
                    this.running = false;
                    this.error = Some(error);
                    cx.notify();
                    return;
                }
                let (payload, pttl) = match parse_dump(&outcome) {
                    Ok(dumped) => dumped,
                    Err(error) => {
                        this.running = false;
                        this.error = Some(error);
                        cx.notify();
                        return;
                    }
                };
                let restore = restore_command(&key, pttl, &payload, replace);
                crate::tool_dialog::run_query(
                    &target_context,
                    restore,
                    cx,
                    move |this, outcome, cx| {
                        this.running = false;
                        if let Some(error) = outcome.error {
                            this.error = Some(if error.contains("BUSYKEY") {
                                crate::localization::text(
                                    "explorer.redis_key_actions.target_exists_replace",
                                )
                            } else {
                                error
                            });
                            cx.notify();
                            return;
                        }
                        cx.emit(RedisKeyActionEvent::Changed {
                            node_id,
                            message: crate::localization::format(
                                "explorer.redis_key_actions.copied_to_connection",
                                &[("key", &key), ("target", &target_name)],
                            ),
                        });
                        cx.emit(RedisKeyActionEvent::Close);
                    },
                );
            }
        });
    }

    fn run_export(&mut self, cx: &mut gpui::Context<Self>) {
        let RedisKeyActionMode::ExportFolder { pattern, node_id } = &self.mode else {
            return;
        };
        let path = std::path::PathBuf::from(self.typed.trim());
        let command = export_command(pattern, self.export_limit());
        let node_id = node_id.clone();
        self.running = true;
        self.error = None;
        cx.notify();
        // Header, one or more lines per key, closing comment.
        let cap = self.export_limit().saturating_mul(1_000).max(10_000);
        crate::tool_dialog::run_query_with_cap(&self.context, command, cap, cx, {
            move |this, outcome, cx| {
                if let Some(error) = outcome.error {
                    this.running = false;
                    this.error = Some(error);
                    cx.notify();
                    return;
                }
                let text = outcome
                    .sets
                    .first()
                    .map(|s| export_text(&s.rows))
                    .unwrap_or_default();
                let summary = export_summary(&text);
                let write = cx.background_spawn({
                    let path = path.clone();
                    async move { std::fs::write(&path, text.as_bytes()) }
                });
                cx.spawn(async move |this, cx| {
                    let result = write.await;
                    this.update(cx, |this, cx| {
                        this.running = false;
                        match result {
                            Ok(()) => {
                                cx.emit(RedisKeyActionEvent::Changed {
                                    node_id,
                                    message: crate::localization::format(
                                        "explorer.redis_key_actions.export_succeeded",
                                        &[
                                            ("path", &path.display().to_string()),
                                            ("summary", &summary),
                                        ],
                                    ),
                                });
                                cx.emit(RedisKeyActionEvent::Close);
                            }
                            Err(error) => {
                                this.error = Some(crate::localization::format(
                                    "explorer.redis_key_actions.export_write_failed",
                                    &[
                                        ("path", &path.display().to_string()),
                                        ("error", &error.to_string()),
                                    ],
                                ));
                                cx.notify();
                            }
                        }
                    })
                    .ok();
                })
                .detach();
            }
        });
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(RedisKeyActionEvent::Close);
    }
}

impl Focusable for RedisKeyActionDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl RedisKeyActionDialog {
    fn note(text: impl Into<gpui::SharedString>, warn: bool) -> gpui::Div {
        div()
            .text_size(px(Theme::font_size_ratio(0.87)))
            .text_color(if warn {
                Theme::warning()
            } else {
                Theme::text_dim()
            })
            .child(text.into())
    }

    fn render_body(&mut self, cx: &mut gpui::Context<Self>) -> gpui::Div {
        let s = Theme::scaled;
        let mut body = div().flex().flex_col().gap(px(s(10.0)));
        match &self.mode {
            RedisKeyActionMode::Rename { .. } => {
                body = body
                    .child(Self::note(
                        crate::localization::text("explorer.redis_key_actions.rename_explanation"),
                        false,
                    ))
                    .child(Input::new(&self.input).w_full())
                    .child(
                        Checkbox::new("redis-rename-nx")
                            .label(crate::localization::text(
                                "explorer.redis_key_actions.rename_only_if_free",
                            ))
                            .checked(self.only_if_free)
                            .on_click(cx.listener(|this, checked: &bool, _w, cx| {
                                this.only_if_free = *checked;
                                this.armed = false;
                                cx.notify();
                            })),
                    );
                if !self.only_if_free {
                    body = body.child(Self::note(
                        crate::localization::text(
                            "explorer.redis_key_actions.rename_overwrite_warning",
                        ),
                        true,
                    ));
                }
            }
            RedisKeyActionMode::CopyToDatabase {
                current_db,
                aliases,
                ..
            } => {
                let target = parse_target_db(&self.typed, Some(*current_db)).ok();
                let named = target
                    .and_then(|db| aliases.get(&db))
                    .map(|alias| format!("db{} — {alias}", target.unwrap_or_default()));
                body = body
                    .child(Self::note(
                        crate::localization::format(
                            "explorer.redis_key_actions.copy_database_explanation",
                            &[(
                                "database",
                                &crate::schema_tree::redis::database_label(*current_db, aliases),
                            )],
                        ),
                        false,
                    ))
                    .child(Input::new(&self.input).w_full());
                if let Some(named) = named {
                    body = body.child(Self::note(named, false));
                }
                body = body.child(self.replace_checkbox(cx));
            }
            RedisKeyActionMode::CopyToConnection { .. } => {
                body = body.child(Self::note(
                    crate::localization::text(
                        "explorer.redis_key_actions.copy_connection_explanation",
                    ),
                    false,
                ));
                if let Some(select) = &self.target_select {
                    body = body.child(
                        Select::new(select)
                            .placeholder(crate::localization::text(
                                "explorer.redis_key_actions.target_connection_placeholder",
                            ))
                            .w_full(),
                    );
                }
                body = body
                    .child(Input::new(&self.input).w_full())
                    .child(self.replace_checkbox(cx));
            }
            RedisKeyActionMode::ExportFolder { pattern, .. } => {
                body = body
                    .child(Self::note(
                        crate::localization::format(
                            "explorer.redis_key_actions.export_explanation",
                            &[("pattern", pattern)],
                        ),
                        false,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(8.0)))
                            .child(Input::new(&self.input).w_full())
                            .child(
                                crate::theme::accent_pill_button(
                                    "redis-export-browse",
                                    crate::localization::text(
                                        "explorer.redis_key_actions.browse_button",
                                    ),
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        let Some(path) = rfd::FileDialog::new()
                                            .add_filter(
                                                crate::localization::text(
                                                    "explorer.redis_key_actions.file_type",
                                                ),
                                                &["redis", "txt"],
                                            )
                                            .set_file_name("keys.redis")
                                            .save_file()
                                        else {
                                            return;
                                        };
                                        let text = path.display().to_string();
                                        this.input.update(cx, |state, cx| {
                                            state.set_value(text, window, cx);
                                        });
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                    .child(Input::new(&self.second_input).w_full());
            }
            RedisKeyActionMode::NameDatabase { db, .. } => {
                body = body
                    .child(Self::note(
                        crate::localization::format(
                            "explorer.redis_key_actions.name_database_explanation",
                            &[("database", &db.to_string())],
                        ),
                        false,
                    ))
                    .child(Input::new(&self.input).w_full());
            }
            RedisKeyActionMode::VisibleDatabases { .. } => {
                body = body
                    .child(Self::note(
                        crate::localization::text(
                            "explorer.redis_key_actions.visible_databases_explanation",
                        ),
                        false,
                    ))
                    .child(Input::new(&self.input).w_full());
                if let Err(error) = crate::schema_tree::redis::parse_visible_databases(&self.typed)
                {
                    body = body.child(Self::note(error, true));
                }
            }
        }
        if let Some(command) = self.command() {
            body = body.child(
                div()
                    .p(px(s(8.0)))
                    .rounded(px(s(4.0)))
                    .bg(Theme::sidebar_bg())
                    .font_family(Theme::editor_font_family())
                    .border_1()
                    .border_color(Theme::border())
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text())
                    .child(command),
            );
        }
        if self.armed {
            body = body.child(Self::note(
                crate::localization::text("explorer.redis_key_actions.confirmation_explanation"),
                true,
            ));
        }
        body
    }

    fn replace_checkbox(&self, cx: &mut gpui::Context<Self>) -> Checkbox {
        Checkbox::new("redis-copy-replace")
            .label(crate::localization::text(
                "explorer.redis_key_actions.replace_checkbox",
            ))
            .checked(self.replace)
            .on_click(cx.listener(|this, checked: &bool, _w, cx| {
                this.replace = *checked;
                this.armed = false;
                cx.notify();
            }))
    }
}

impl Render for RedisKeyActionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let titled = |key, name: &String| crate::localization::format(key, &[("name", name)]);
        let (title, label, danger) = match &self.mode {
            RedisKeyActionMode::Rename { key, .. } => (
                titled("explorer.redis_key_actions.rename_title", key),
                crate::localization::text("explorer.redis_key_actions.rename_button"),
                !self.only_if_free,
            ),
            RedisKeyActionMode::CopyToDatabase { key, .. } => (
                titled("explorer.redis_key_actions.copy_database_title", key),
                crate::localization::text("explorer.redis_key_actions.copy_button"),
                self.replace,
            ),
            RedisKeyActionMode::CopyToConnection { key, .. } => (
                titled("explorer.redis_key_actions.copy_connection_title", key),
                crate::localization::text("explorer.redis_key_actions.copy_button"),
                self.replace,
            ),
            RedisKeyActionMode::ExportFolder { .. } => (
                crate::localization::text("explorer.redis_key_actions.export_title"),
                crate::localization::text("explorer.redis_key_actions.export_button"),
                false,
            ),
            RedisKeyActionMode::NameDatabase { db, .. } => (
                titled(
                    "explorer.redis_key_actions.name_database_title",
                    &db.to_string(),
                ),
                crate::localization::text("explorer.redis_key_actions.save_button"),
                false,
            ),
            RedisKeyActionMode::VisibleDatabases { .. } => (
                crate::localization::text("explorer.redis_key_actions.visible_databases_title"),
                crate::localization::text("explorer.redis_key_actions.save_button"),
                false,
            ),
        };
        let label = if self.armed {
            crate::localization::text("explorer.redis_key_actions.confirm_run_button")
        } else {
            label
        };
        // The key and export dialogs name the database they act on.
        let title =
            if self.mode.writes() || matches!(self.mode, RedisKeyActionMode::ExportFolder { .. }) {
                crate::localization::format(
                    "explorer.redis_key_actions.title_with_database",
                    &[
                        ("title", &title),
                        (
                            "database",
                            &crate::schema_tree::redis::database_label_text(
                                self.context.profile.database.as_deref().unwrap_or("0"),
                                &self.context.profile,
                            ),
                        ),
                    ],
                )
            } else {
                title
            };
        let mut body = self.render_body(cx);
        if let Some(reason) = &self.write_block {
            body = body.child(Self::note(reason.clone(), true));
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::error())
                    .child(error.clone()),
            );
        }
        let focus_handle = self.focus_handle.clone();
        let allowed = self.confirm_allowed();
        let running = self.running;
        Dialog::new("redis-key-action-dialog", |this: &mut Self| &mut this.frame)
            .title(title)
            .easy_close(!running)
            .easy_close_when_dirty(!running)
            .key_context("RedisKeyActionDialog")
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            .child(body)
            .button(
                Button::new("redis-key-action-cancel")
                    .label(crate::localization::text(
                        "explorer.redis_key_actions.cancel_button",
                    ))
                    .disabled(running)
                    .on_click(cx.listener(|this, _, _window, cx| this.close(cx))),
            )
            .button({
                let button = Button::new("redis-key-action-confirm")
                    .label(label)
                    .disabled(!allowed);
                let button = if danger {
                    button.danger()
                } else {
                    button.primary()
                };
                button.on_click(cx.listener(|this, _, _window, cx| this.confirm(cx)))
            })
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::query::safety::redis::{CommandAccess, command_access, parse_script};

    fn access(text: &str) -> Vec<CommandAccess> {
        parse_script(text)
            .unwrap()
            .iter()
            .map(command_access)
            .collect()
    }

    #[test]
    fn redis_key_actions_rename_prefers_nx_and_quotes_bytes() {
        assert_eq!(
            rename_command("user:1", "user:2", true),
            "RENAMENX user:1 user:2"
        );
        assert_eq!(
            rename_command("a b", "c\"d", false),
            "RENAME \"a b\" \"c\\\"d\""
        );
        // Both are reviewed as destructive by the shared allowlist.
        assert_eq!(
            access(&rename_command("a", "b", true)),
            vec![CommandAccess::Destructive]
        );
    }

    #[test]
    fn redis_key_actions_copy_to_database_and_db_parsing() {
        assert_eq!(copy_to_database_command("k", 3, false), "COPY k k DB 3");
        assert_eq!(
            copy_to_database_command("k", 3, true),
            "COPY k k DB 3 REPLACE"
        );
        assert_eq!(parse_target_db(" 4 ", Some(0)), Ok(4));
        assert!(parse_target_db("2", Some(2)).is_err());
        assert!(parse_target_db("x", None).is_err());
        assert!(parse_target_db("-1", None).is_err());
    }

    #[test]
    fn redis_key_actions_restore_round_trips_payload_bytes() {
        let payload = vec![0x00, 0x0b, 0xff, b'"', b'\\', 0x09, 0x00];
        let text = restore_command("wsk:k", -1, &payload, true);
        let parsed = parse_script(&text).unwrap();
        assert_eq!(
            parsed[0].args,
            vec![
                b"RESTORE".to_vec(),
                b"wsk:k".to_vec(),
                b"0".to_vec(),
                payload,
                b"REPLACE".to_vec()
            ]
        );
        assert_eq!(dump_script("k"), "DUMP k\nPTTL k");
        assert_eq!(access(&dump_script("k")), vec![CommandAccess::ReadOnly; 2]);
    }

    #[test]
    fn redis_key_actions_parse_dump_reads_payload_and_ttl() {
        let set = |cell: CellValue| crate::tool_dialog::ToolQuerySet {
            columns: Vec::new(),
            rows: vec![vec![cell]],
        };
        let outcome = ToolQueryOutcome {
            sets: vec![set(CellValue::Bytes(vec![1, 2])), set(CellValue::I64(-1))],
            error: None,
        };
        assert_eq!(parse_dump(&outcome), Ok((vec![1, 2], 0)));
        let ttl = ToolQueryOutcome {
            sets: vec![set(CellValue::Bytes(vec![9])), set(CellValue::I64(1500))],
            error: None,
        };
        assert_eq!(parse_dump(&ttl), Ok((vec![9], 1500)));
        let gone = ToolQueryOutcome {
            sets: vec![set(CellValue::Null), set(CellValue::I64(-2))],
            error: None,
        };
        assert!(parse_dump(&gone).is_err());
    }

    #[test]
    fn redis_key_actions_export_command_text_and_summary() {
        assert_eq!(
            export_command("session:*", 0),
            "SQLLY.DUMPSCRIPT session:* 1"
        );
        assert_eq!(
            export_command("a b*", 500_000),
            format!("SQLLY.DUMPSCRIPT \"a b*\" {EXPORT_MAX_LIMIT}")
        );
        assert_eq!(
            access(&export_command("x:*", 5)),
            vec![CommandAccess::ReadOnly]
        );
        let rows = vec![
            vec![CellValue::String("# header".into())],
            vec![CellValue::String("SET k v".into())],
            vec![CellValue::String(
                "# complete: 1 keys exported, 0 skipped".into(),
            )],
        ];
        let text = export_text(&rows);
        assert_eq!(
            text,
            "# header\nSET k v\n# complete: 1 keys exported, 0 skipped\n"
        );
        assert_eq!(
            export_summary(&text),
            "complete: 1 keys exported, 0 skipped"
        );
        // The file the export writes is accepted by the import's decoder.
        assert!(crate::redis_bulk::decode_import(text.as_bytes()).is_ok());
    }

    #[test]
    fn redis_key_actions_export_fits_the_import_cap() {
        assert_eq!(
            sqlly_engines::redis::dumpscript::SCRIPT_BYTE_BUDGET as u64,
            crate::redis_key_dialog::IMPORT_MAX_BYTES,
            "an export must always be importable"
        );
    }

    #[test]
    fn redis_key_actions_refuse_writes_on_read_only_or_locked_production() {
        let mut profile = ConnectionProfile::sql_login(
            "r",
            "h",
            "",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = sqlly_core::DatabaseType::Redis;
        assert!(write_block(&profile, ProductionDecision::NotProduction).is_none());
        assert!(write_block(&profile, ProductionDecision::ReadOnly).is_some());
        profile.block_all_but_select = true;
        assert!(write_block(&profile, ProductionDecision::NotProduction).is_some());
    }
}
