//! The Security tool's page for Redis: ACL users and the ACL log.
//!
//! `ACL LIST` is parsed into one row per user — enabled, how many
//! passwords, key patterns (`~`, `%R~`, `%W~`, `%RW~`), channel patterns
//! (`&`), command rules (`+@read -flushall …`) and Redis 7 selectors
//! (`(…)` groups) — and `ACL LOG` into a second grid of recent denials.
//! Add user…, Edit… and Delete… never run anything: they build a reviewed
//! `ACL SETUSER` / `ACL DELUSER` script and hand it to a new query tab
//! pinned to this connection, the admin-dialog contract in `tool_dialog`
//! (the editor's run gate then applies — both commands are classified
//! destructive). A new password is written as its SHA-256 (`#<hash>`), so
//! the clear text never lands in the tab, the history, or the server's
//! logs; an edit keeps the user's existing password hashes unless asked to
//! replace them. The page only reads (`ACL LIST`, `ACL LOG`).

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use sha2::{Digest, Sha256};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::SqllyDataTable;
use sqlly_intel::query::safety::redis::quote_argument;

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent, ToolQuerySet};

/// Recent `ACL LOG` entries the page reads.
pub const ACL_LOG_ENTRIES: usize = 50;

/// One parsed `ACL LIST` line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AclUser {
    pub name: String,
    pub enabled: bool,
    /// `nopass`: any password (or none) signs in.
    pub nopass: bool,
    /// The `#<sha256>` password hashes, kept so an edit can re-add them.
    pub password_hashes: Vec<String>,
    /// `~pattern`, `%R~pattern`, `allkeys`, … in server order.
    pub keys: Vec<String>,
    /// `&pattern`, `allchannels`, `resetchannels`.
    pub channels: Vec<String>,
    /// `+@all`, `-@dangerous`, `+get`, …
    pub commands: Vec<String>,
    /// Redis 7 selectors, each with its parentheses: `(~x:* +get)`.
    pub selectors: Vec<String>,
    /// Anything else (`sanitize-payload`, …), kept verbatim.
    pub flags: Vec<String>,
}

/// Split an ACL rule string on spaces, keeping each `( … )` selector as one
/// token.
pub fn acl_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn is_key_rule(token: &str) -> bool {
    token.starts_with('~') || token.starts_with('%') || matches!(token, "allkeys" | "resetkeys")
}

fn is_channel_rule(token: &str) -> bool {
    token.starts_with('&') || matches!(token, "allchannels" | "resetchannels")
}

fn is_command_rule(token: &str) -> bool {
    token.starts_with('+')
        || token.starts_with('-')
        || matches!(token, "allcommands" | "nocommands")
}

/// Parse one `ACL LIST` line (`user <name> <rules…>`).
pub fn parse_acl_line(line: &str) -> Option<AclUser> {
    let tokens = acl_tokens(line);
    let mut iter = tokens.into_iter();
    if iter.next()?.as_str() != "user" {
        return None;
    }
    let mut user = AclUser {
        name: iter.next()?,
        ..Default::default()
    };
    for token in iter {
        match token.as_str() {
            "on" => user.enabled = true,
            "off" => user.enabled = false,
            "nopass" => user.nopass = true,
            t if t.starts_with('#') => user.password_hashes.push(token),
            t if t.starts_with('(') => user.selectors.push(token),
            t if is_key_rule(t) => user.keys.push(token),
            t if is_channel_rule(t) => user.channels.push(token),
            t if is_command_rule(t) => user.commands.push(token),
            _ => user.flags.push(token),
        }
    }
    Some(user)
}

/// The users from `ACL LIST`'s rows (each row's string cell is one line).
pub fn parse_acl_list(rows: &[Vec<CellValue>]) -> Vec<AclUser> {
    rows.iter()
        .filter_map(|row| {
            row.iter().find_map(|cell| match cell {
                CellValue::String(s) if s.starts_with("user ") => parse_acl_line(s),
                _ => None,
            })
        })
        .collect()
}

fn column(name: &str, data_type: &str) -> ResultColumn {
    ResultColumn {
        name: name.to_string(),
        data_type: data_type.to_string(),
        nullable: true,
    }
}

/// The users grid's set.
pub fn users_set(users: &[AclUser]) -> ToolQuerySet {
    ToolQuerySet {
        columns: vec![
            column("user", "string"),
            column("enabled", "boolean"),
            column("passwords", "string"),
            column("keys", "string"),
            column("channels", "string"),
            column("commands", "string"),
            column("selectors", "string"),
        ],
        rows: users
            .iter()
            .map(|u| {
                vec![
                    CellValue::String(u.name.clone()),
                    CellValue::Bool(u.enabled),
                    CellValue::String(if u.nopass {
                        crate::localization::text("explorer.redis_acl.no_password_needed")
                    } else {
                        u.password_hashes.len().to_string()
                    }),
                    CellValue::String(u.keys.join(" ")),
                    CellValue::String(u.channels.join(" ")),
                    CellValue::String(u.commands.join(" ")),
                    CellValue::String(u.selectors.join(" ")),
                ]
            })
            .collect(),
    }
}

/// `ACL LOG` rows (each a flat JSON array of field/value pairs under
/// RESP2, an object under RESP3) as a grid set.
pub fn log_set(rows: &[Vec<CellValue>]) -> ToolQuerySet {
    const FIELDS: [&str; 7] = [
        "count",
        "reason",
        "context",
        "object",
        "username",
        "age-seconds",
        "client-info",
    ];
    let entries = rows.iter().filter_map(|row| {
        let text = row.iter().rev().find_map(|cell| match cell {
            CellValue::String(s) if s.starts_with('[') || s.starts_with('{') => Some(s.clone()),
            _ => None,
        })?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        let lookup = |name: &str| -> Option<serde_json::Value> {
            match &value {
                serde_json::Value::Object(map) => map.get(name).cloned(),
                serde_json::Value::Array(items) => items
                    .chunks(2)
                    .find(|pair| pair.first().and_then(|k| k.as_str()) == Some(name))
                    .and_then(|pair| pair.get(1).cloned()),
                _ => None,
            }
        };
        Some(
            FIELDS
                .iter()
                .map(|field| match lookup(field) {
                    Some(serde_json::Value::String(s)) => CellValue::String(s),
                    Some(serde_json::Value::Number(n)) => CellValue::String(n.to_string()),
                    Some(serde_json::Value::Null) | None => CellValue::Null,
                    Some(other) => CellValue::String(other.to_string()),
                })
                .collect::<Vec<_>>(),
        )
    });
    ToolQuerySet {
        columns: FIELDS
            .iter()
            .map(|f| column(&f.replace('-', "_"), "string"))
            .collect(),
        rows: entries.collect(),
    }
}

/// `#<sha256 hex>` for a clear-text password, the form `ACL SETUSER`
/// accepts so the password itself is never written down.
pub fn password_hash_rule(password: &str) -> String {
    let digest = Sha256::digest(password.as_bytes());
    let mut out = String::with_capacity(65);
    out.push('#');
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// What the Add / Edit form holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AclForm {
    pub name: String,
    pub enabled: bool,
    /// A new password to add (blank: none).
    pub new_password: String,
    /// Edit: keep the user's existing password hashes.
    pub keep_passwords: bool,
    /// The user's existing hashes (edit only).
    pub existing_hashes: Vec<String>,
    /// No password needed (`nopass`) when no password is set.
    pub nopass: bool,
    pub keys: String,
    pub channels: String,
    pub commands: String,
    pub selectors: String,
    pub flags: Vec<String>,
}

impl AclForm {
    pub fn for_user(user: &AclUser) -> Self {
        AclForm {
            name: user.name.clone(),
            enabled: user.enabled,
            new_password: String::new(),
            keep_passwords: true,
            existing_hashes: user.password_hashes.clone(),
            nopass: user.nopass,
            keys: user.keys.join(" "),
            channels: user.channels.join(" "),
            commands: user.commands.join(" "),
            selectors: user.selectors.join(" "),
            flags: user.flags.clone(),
        }
    }

    pub fn new_user() -> Self {
        AclForm {
            enabled: true,
            keep_passwords: true,
            keys: "~*".into(),
            channels: "&*".into(),
            commands: "+@read".into(),
            ..Default::default()
        }
    }
}

/// Why a form cannot be scripted, or the `ACL SETUSER` line. The user is
/// rebuilt from scratch (`reset`) so the script states the whole
/// definition — what you review is exactly what the user will be.
pub fn setuser_script(form: &AclForm) -> Result<String, String> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err(crate::localization::text(
            "explorer.redis_acl.user_name_required",
        ));
    }
    if name.chars().any(char::is_whitespace) {
        return Err(crate::localization::text(
            "explorer.redis_acl.user_name_no_spaces",
        ));
    }
    let mut rules: Vec<String> = vec!["reset".into()];
    rules.push(if form.enabled { "on" } else { "off" }.into());
    let mut has_password = false;
    if form.keep_passwords {
        for hash in &form.existing_hashes {
            rules.push(hash.clone());
            has_password = true;
        }
    }
    if !form.new_password.is_empty() {
        rules.push(password_hash_rule(&form.new_password));
        has_password = true;
    }
    if !has_password && form.nopass {
        rules.push("nopass".into());
    }
    for (key, text, check) in [
        (
            "explorer.redis_acl.invalid_key_pattern",
            &form.keys,
            is_key_rule as fn(&str) -> bool,
        ),
        (
            "explorer.redis_acl.invalid_channel_pattern",
            &form.channels,
            is_channel_rule,
        ),
        (
            "explorer.redis_acl.invalid_command_rule",
            &form.commands,
            is_command_rule,
        ),
    ] {
        for token in acl_tokens(text) {
            if !check(&token) {
                return Err(crate::localization::format(key, &[("token", &token)]));
            }
            rules.push(token);
        }
    }
    for selector in acl_tokens(&form.selectors) {
        if !(selector.starts_with('(') && selector.ends_with(')')) {
            return Err(crate::localization::format(
                "explorer.redis_acl.invalid_selector",
                &[("selector", &selector)],
            ));
        }
        rules.push(selector);
    }
    rules.extend(form.flags.iter().cloned());
    let mut line = format!("ACL SETUSER {}", quote_argument(name.as_bytes()));
    for rule in rules {
        line.push(' ');
        line.push_str(&quote_argument(rule.as_bytes()));
    }
    Ok(line)
}

/// The reviewed script for a form: a comment saying what it does, the
/// `ACL SETUSER` line, and `ACL GETUSER` to show the result.
pub fn setuser_review_script(form: &AclForm, editing: bool) -> Result<String, String> {
    let line = setuser_script(form)?;
    let name = quote_argument(form.name.trim().as_bytes());
    Ok(format!(
        "# {} Redis ACL user {} — the rules below replace the whole definition (reset, then each rule). New passwords are written as SHA-256 hashes.\n{line}\nACL GETUSER {name}\n",
        if editing { "Update" } else { "Create" },
        form.name.trim()
    ))
}

/// `ACL DELUSER name` with a comment; the `default` user cannot be removed.
pub fn deluser_script(name: &str) -> Result<String, String> {
    if name == "default" {
        return Err(crate::localization::text(
            "explorer.redis_acl.default_user_delete_forbidden",
        ));
    }
    Ok(format!(
        "# Delete Redis ACL user {name}: its connections are closed and it can no longer sign in.\nACL DELUSER {}\n",
        quote_argument(name.as_bytes())
    ))
}

/// The read the page runs: users, then the recent denials.
pub fn page_script() -> String {
    format!("ACL LIST\nACL LOG {ACL_LOG_ENTRIES}")
}

/// Which form, if any, is open.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FormMode {
    Add,
    Edit,
}

pub struct RedisAclPane {
    context: ToolDialogContext,
    users: Vec<AclUser>,
    users_grid: Option<Entity<SqllyDataTable>>,
    log_grid: Option<Entity<SqllyDataTable>>,
    log_rows: usize,
    loading: bool,
    error: Option<String>,
    form_mode: Option<FormMode>,
    form: AclForm,
    inputs: [Entity<InputState>; 6],
    form_error: Option<String>,
    generation: u64,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ToolDialogEvent> for RedisAclPane {}

/// Form field order in `inputs`.
const NAME: usize = 0;
const PASSWORD: usize = 1;
const KEYS: usize = 2;
const CHANNELS: usize = 3;
const COMMANDS: usize = 4;
const SELECTORS: usize = 5;

impl RedisAclPane {
    pub fn new(
        context: ToolDialogContext,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let placeholders = [
            "explorer.redis_acl.user_name_placeholder",
            "explorer.redis_acl.password_placeholder",
            "explorer.redis_acl.keys_placeholder",
            "explorer.redis_acl.channels_placeholder",
            "explorer.redis_acl.commands_placeholder",
            "explorer.redis_acl.selectors_placeholder",
        ];
        let inputs = placeholders.map(|key| {
            cx.new(|cx| {
                let state = InputState::new(window, cx)
                    .placeholder(crate::localization::input_placeholder(window, cx, key));
                if key == "explorer.redis_acl.password_placeholder" {
                    state.masked(true)
                } else {
                    state
                }
            })
        });
        let subscriptions = inputs
            .iter()
            .map(|input| {
                cx.subscribe_in(
                    input,
                    window,
                    |this: &mut Self, _, event: &InputEvent, _, cx| {
                        if let InputEvent::Change = event {
                            this.form_error = None;
                            cx.notify();
                        }
                    },
                )
            })
            .collect();
        let mut this = Self {
            context,
            users: Vec::new(),
            users_grid: None,
            log_grid: None,
            log_rows: 0,
            loading: true,
            error: None,
            form_mode: None,
            form: AclForm::new_user(),
            inputs,
            form_error: None,
            generation: 0,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(900.0), Theme::scaled(640.0)),
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        self.loading = true;
        self.generation += 1;
        let generation = self.generation;
        cx.notify();
        crate::tool_dialog::run_query(
            &self.context,
            page_script(),
            cx,
            move |this, outcome, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                this.error = outcome.error.map(|error| {
                    if error.contains("NOPERM") {
                        crate::localization::format(
                            "explorer.redis_acl.permission_denied",
                            &[("error", &error)],
                        )
                    } else {
                        error
                    }
                });
                if let Some(set) = outcome.sets.first() {
                    this.users = parse_acl_list(&set.rows);
                    this.users_grid =
                        crate::tool_dialog::build_result_grid(&users_set(&this.users), cx);
                }
                if let Some(set) = outcome.sets.get(1) {
                    let log = log_set(&set.rows);
                    this.log_rows = log.rows.len();
                    this.log_grid = crate::tool_dialog::build_result_grid(&log, cx);
                }
                cx.notify();
            },
        );
    }

    fn selected_user(&self, cx: &gpui::App) -> Option<AclUser> {
        let grid = self.users_grid.as_ref()?;
        let row = crate::tool_dialog::selected_source_row(grid, cx)?;
        self.users.get(row).cloned()
    }

    fn open_form(
        &mut self,
        mode: FormMode,
        form: AclForm,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let values = [
            form.name.clone(),
            String::new(),
            form.keys.clone(),
            form.channels.clone(),
            form.commands.clone(),
            form.selectors.clone(),
        ];
        for (input, value) in self.inputs.iter().zip(values) {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        self.form = form;
        self.form_mode = Some(mode);
        self.form_error = None;
        cx.notify();
    }

    fn read_form(&self, cx: &gpui::App) -> AclForm {
        let text = |i: usize| self.inputs[i].read(cx).value().to_string();
        AclForm {
            name: text(NAME),
            new_password: text(PASSWORD),
            keys: text(KEYS),
            channels: text(CHANNELS),
            commands: text(COMMANDS),
            selectors: text(SELECTORS),
            ..self.form.clone()
        }
    }

    fn script_form(&mut self, cx: &mut gpui::Context<Self>) {
        let form = self.read_form(cx);
        let editing = self.form_mode == Some(FormMode::Edit);
        match setuser_review_script(&form, editing) {
            Ok(sql) => {
                cx.emit(ToolDialogEvent::ScriptToEditorForProfile {
                    sql,
                    profile: Box::new(self.context.profile.clone()),
                });
                self.form_mode = None;
                cx.notify();
            }
            Err(error) => {
                self.form_error = Some(error);
                cx.notify();
            }
        }
    }

    fn script_delete(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(user) = self.selected_user(cx) else {
            self.error = Some(crate::localization::text(
                "explorer.redis_acl.select_user_first",
            ));
            cx.notify();
            return;
        };
        match deluser_script(&user.name) {
            Ok(sql) => cx.emit(ToolDialogEvent::ScriptToEditorForProfile {
                sql,
                profile: Box::new(self.context.profile.clone()),
            }),
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn render_form(&self, cx: &mut gpui::Context<Self>) -> Option<gpui::AnyElement> {
        let mode = self.form_mode.as_ref()?;
        let s = Theme::scaled;
        let label = |text: &str| {
            div()
                .w(px(s(110.0)))
                .text_size(px(Theme::font_size_ratio(0.86)))
                .text_color(Theme::text_dim())
                .child(text.to_string())
        };
        let row = |name: &'static str, input: &Entity<InputState>| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .child(label(&crate::localization::text(name)))
                .child(Input::new(input).w_full())
        };
        let editing = *mode == FormMode::Edit;
        let mut form = div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .p(px(s(10.0)))
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(4.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text())
                    .child(crate::localization::text(if editing {
                        "explorer.redis_acl.edit_user_title"
                    } else {
                        "explorer.redis_acl.add_user_title"
                    })),
            )
            .child(row("explorer.redis_acl.user_label", &self.inputs[NAME]))
            .child(row(
                "explorer.redis_acl.password_label",
                &self.inputs[PASSWORD],
            ))
            .child(row("explorer.redis_acl.keys_label", &self.inputs[KEYS]))
            .child(row(
                "explorer.redis_acl.channels_label",
                &self.inputs[CHANNELS],
            ))
            .child(row(
                "explorer.redis_acl.commands_label",
                &self.inputs[COMMANDS],
            ))
            .child(row(
                "explorer.redis_acl.selectors_label",
                &self.inputs[SELECTORS],
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(16.0)))
                    .child(
                        Checkbox::new("redis-acl-enabled")
                            .label(crate::localization::text(
                                "explorer.redis_acl.enabled_label",
                            ))
                            .checked(self.form.enabled)
                            .on_click(cx.listener(|this, checked: &bool, _w, cx| {
                                this.form.enabled = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("redis-acl-nopass")
                            .label(crate::localization::text("explorer.redis_acl.nopass_label"))
                            .checked(self.form.nopass)
                            .on_click(cx.listener(|this, checked: &bool, _w, cx| {
                                this.form.nopass = *checked;
                                cx.notify();
                            })),
                    )
                    .when(editing, |d| {
                        d.child(
                            Checkbox::new("redis-acl-keep")
                                .label(crate::localization::format(
                                    "explorer.redis_acl.keep_passwords",
                                    &[("count", &self.form.existing_hashes.len().to_string())],
                                ))
                                .checked(self.form.keep_passwords)
                                .on_click(cx.listener(|this, checked: &bool, _w, cx| {
                                    this.form.keep_passwords = *checked;
                                    cx.notify();
                                })),
                        )
                    }),
            );
        if let Some(error) = &self.form_error {
            form = form.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::error())
                    .child(error.clone()),
            );
        }
        form = form.child(
            div()
                .flex()
                .flex_row()
                .gap(px(s(8.0)))
                .child(
                    Button::new("redis-acl-script")
                        .label(crate::localization::text(
                            "explorer.redis_acl.script_button",
                        ))
                        .primary()
                        .on_click(cx.listener(|this, _, _w, cx| this.script_form(cx))),
                )
                .child(
                    Button::new("redis-acl-form-cancel")
                        .label(crate::localization::text(
                            "explorer.redis_acl.cancel_form_button",
                        ))
                        .on_click(cx.listener(|this, _, _w, cx| {
                            this.form_mode = None;
                            cx.notify();
                        })),
                ),
        );
        Some(form.into_any_element())
    }

    fn grid_section(
        title: String,
        grid: Option<&Entity<SqllyDataTable>>,
        empty: &str,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        let body = match grid {
            Some(grid) => div().size_full().child(grid.clone().into_any_element()),
            None => div()
                .p(px(s(8.0)))
                .text_size(px(Theme::font_size_ratio(0.92)))
                .text_color(Theme::text_dim())
                .child(empty.to_string()),
        };
        div()
            .flex()
            .flex_col()
            .flex_basis(px(0.0))
            .flex_1()
            .min_h(px(s(110.0)))
            .gap(px(s(4.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.86)))
                    .text_color(Theme::text())
                    .child(title),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .child(body),
            )
            .into_any_element()
    }
}

impl gpui::Focusable for RedisAclPane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RedisAclPane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let focus_handle = self.focus_handle.clone();
        let form = self.render_form(cx);
        let empty_users = crate::localization::text(if self.loading {
            "explorer.redis_acl.users_loading"
        } else {
            "explorer.redis_acl.users_empty"
        });
        let empty_log = crate::localization::text(if self.loading {
            "explorer.redis_acl.log_loading"
        } else {
            "explorer.redis_acl.log_empty"
        });
        Dialog::new("redis-acl-pane", |this: &mut Self| &mut this.frame)
            .title(crate::localization::format(
                "explorer.redis_acl.title",
                &[("connection", &self.context.profile.name)],
            ))
            .easy_close(true)
            .key_context("RedisAclPane")
            .track_focus(&focus_handle)
            .on_close(|_this, _window, cx| cx.emit(ToolDialogEvent::Close))
            .bare_body()
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(s(8.0)))
                    .p(px(s(14.0)))
                    .children(self.error.clone().map(|err| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .text_color(Theme::error())
                            .child(err)
                    }))
                    .child(Self::grid_section(
                        crate::localization::format(
                            "explorer.redis_acl.users_section",
                            &[("count", &self.users.len().to_string())],
                        ),
                        self.users_grid.as_ref(),
                        &empty_users,
                    ))
                    .children(form)
                    .child(Self::grid_section(
                        crate::localization::format(
                            "explorer.redis_acl.log_section",
                            &[
                                ("limit", &ACL_LOG_ENTRIES.to_string()),
                                ("count", &self.log_rows.to_string()),
                            ],
                        ),
                        self.log_grid.as_ref(),
                        &empty_log,
                    ))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "explorer.redis_acl.review_explanation",
                            )),
                    ),
            )
            .leading_button(
                Button::new("redis-acl-refresh")
                    .label(crate::localization::text(
                        "explorer.redis_acl.refresh_button",
                    ))
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _w, cx| this.refresh(cx))),
            )
            .leading_button(
                Button::new("redis-acl-add")
                    .label(crate::localization::text("explorer.redis_acl.add_button"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_form(FormMode::Add, AclForm::new_user(), window, cx)
                    })),
            )
            .leading_button(
                Button::new("redis-acl-edit")
                    .label(crate::localization::text("explorer.redis_acl.edit_button"))
                    .on_click(
                        cx.listener(|this, _, window, cx| match this.selected_user(cx) {
                            Some(user) => {
                                this.open_form(FormMode::Edit, AclForm::for_user(&user), window, cx)
                            }
                            None => {
                                this.error = Some(crate::localization::text(
                                    "explorer.redis_acl.select_user_first",
                                ));
                                cx.notify();
                            }
                        }),
                    ),
            )
            .leading_button(
                Button::new("redis-acl-delete")
                    .label(crate::localization::text(
                        "explorer.redis_acl.delete_button",
                    ))
                    .danger()
                    .on_click(cx.listener(|this, _, _w, cx| this.script_delete(cx))),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::query::safety::redis::{CommandAccess, command_access, parse_script};

    const LIST: [&str; 3] = [
        "user default on nopass sanitize-payload ~* &* +@all",
        "user wsk_app on #5e884898da28047151d0e56f8dc6292773603d0d6aabbdd62a11ef721d1542d8 ~app:* %R~config:* resetchannels &events:* -@all +@read +set (~cache:* %W~tmp:* +get +set) (&audit:* +publish)",
        "user wsk_off off resetchannels -@all",
    ];

    fn rows() -> Vec<Vec<CellValue>> {
        LIST.iter()
            .enumerate()
            .map(|(i, l)| vec![CellValue::I64(i as i64), CellValue::String((*l).into())])
            .collect()
    }

    #[test]
    fn redis_acl_list_parses_users_patterns_and_selectors() {
        let users = parse_acl_list(&rows());
        assert_eq!(users.len(), 3);
        let default = &users[0];
        assert!(default.enabled && default.nopass);
        assert_eq!(default.keys, vec!["~*"]);
        assert_eq!(default.channels, vec!["&*"]);
        assert_eq!(default.commands, vec!["+@all"]);
        assert_eq!(default.flags, vec!["sanitize-payload"]);

        let app = &users[1];
        assert_eq!(app.name, "wsk_app");
        assert!(app.enabled && !app.nopass);
        assert_eq!(app.password_hashes.len(), 1);
        assert_eq!(app.keys, vec!["~app:*", "%R~config:*"]);
        assert_eq!(app.channels, vec!["resetchannels", "&events:*"]);
        assert_eq!(app.commands, vec!["-@all", "+@read", "+set"]);
        assert_eq!(
            app.selectors,
            vec!["(~cache:* %W~tmp:* +get +set)", "(&audit:* +publish)"]
        );
        assert!(!users[2].enabled);

        let set = users_set(&users);
        assert_eq!(set.rows[1][2], CellValue::String("1".into()));
        assert_eq!(
            set.rows[0][2],
            CellValue::String("none needed (nopass)".into())
        );
        assert_eq!(set.columns.len(), set.rows[0].len());
    }

    #[test]
    fn redis_acl_edit_round_trips_the_definition_and_keeps_hashes() {
        let users = parse_acl_list(&rows());
        let form = AclForm::for_user(&users[1]);
        let line = setuser_script(&form).unwrap();
        let args = &parse_script(&line).unwrap()[0].args;
        let text: Vec<String> = args
            .iter()
            .map(|a| String::from_utf8(a.clone()).unwrap())
            .collect();
        assert_eq!(&text[..4], ["ACL", "SETUSER", "wsk_app", "reset"]);
        assert!(text.contains(&"on".to_string()));
        assert!(text.contains(&users[1].password_hashes[0]));
        assert!(text.contains(&"(~cache:* %W~tmp:* +get +set)".to_string()));
        assert!(text.contains(&"%R~config:*".to_string()));
        // Re-parsing the scripted rules gives the same user back.
        let reparsed = parse_acl_line(&format!(
            "user {}",
            text[2..].join(" ").replacen("reset ", "", 1)
        ))
        .unwrap();
        assert_eq!(reparsed.keys, users[1].keys);
        assert_eq!(reparsed.selectors, users[1].selectors);
        assert_eq!(reparsed.password_hashes, users[1].password_hashes);
        assert_eq!(
            command_access(&parse_script(&line).unwrap()[0]),
            CommandAccess::Destructive
        );
    }

    #[test]
    fn redis_acl_new_password_is_hashed_never_written() {
        let mut form = AclForm::new_user();
        form.name = "wsk_new".into();
        form.new_password = "s3cret pass".into();
        let script = setuser_review_script(&form, false).unwrap();
        assert!(!script.contains("s3cret"), "{script}");
        assert!(script.contains(&password_hash_rule("s3cret pass")));
        assert_eq!(password_hash_rule("password").len(), 65);
        assert_eq!(
            password_hash_rule("password"),
            "#5e884898da28047151d0e56f8dc6292773603d0d6aabbdd62a11ef721d1542d8"
        );
        assert!(script.contains("ACL GETUSER wsk_new"));
        // Dropping the old hashes and setting none falls back to nopass only
        // when asked.
        let mut edit = AclForm::for_user(&parse_acl_list(&rows())[1]);
        edit.keep_passwords = false;
        edit.nopass = false;
        let line = setuser_script(&edit).unwrap();
        assert!(!line.contains('#') && !line.contains("nopass"), "{line}");
        edit.nopass = true;
        assert!(setuser_script(&edit).unwrap().contains("nopass"));
    }

    #[test]
    fn redis_acl_form_rejects_misfiled_rules() {
        let mut form = AclForm::new_user();
        form.name = "wsk x".into();
        assert!(setuser_script(&form).is_err());
        form.name = "wsk_x".into();
        form.keys = "app:*".into();
        assert!(setuser_script(&form).unwrap_err().contains("key pattern"));
        form.keys = "~app:*".into();
        form.commands = "get".into();
        assert!(setuser_script(&form).unwrap_err().contains("command rule"));
        form.commands = "+get".into();
        form.selectors = "~x".into();
        assert!(setuser_script(&form).unwrap_err().contains("selector"));
    }

    /// Live: the scripts the page writes, run exactly as a query tab would,
    /// create, edit and delete a `wsk_*` user; the page's parser reads the
    /// server's own ACL LIST back, selectors included.
    #[tokio::test]
    async fn redis_acl_live_create_edit_delete() {
        let Some(profile) = sqlly_engines::redis::live_profile_from_env("wsk-acl") else {
            eprintln!("SQLLY_REDIS_URL not set; skipping");
            return;
        };
        let client = sqlly_engines::redis::open_client(&profile)
            .await
            .expect("open");
        let run = |text: String| {
            let client = &client;
            async move {
                sqlly_engines::redis::query_rows_on_client(client, &text)
                    .await
                    .expect("script")
            }
        };
        let list = || async {
            parse_acl_list(
                &run(page_script().lines().next().unwrap().to_string())
                    .await
                    .rows,
            )
        };
        let name = format!("wsk_acl_{}", std::process::id());
        let _ = sqlly_engines::redis::query_rows_on_client(&client, &format!("ACL DELUSER {name}"))
            .await;

        let mut form = AclForm::new_user();
        form.name = name.clone();
        form.new_password = "wsk-pass".into();
        form.keys = "~wsk:* %R~cfg:*".into();
        form.channels = "&wsk:*".into();
        form.commands = "-@all +@read +set".into();
        form.selectors = "(~cache:* +get)".into();
        run(setuser_review_script(&form, false).unwrap()).await;
        let created = list()
            .await
            .into_iter()
            .find(|u| u.name == name)
            .expect("created");
        assert!(created.enabled);
        assert_eq!(
            created.password_hashes,
            vec![password_hash_rule("wsk-pass")]
        );
        assert_eq!(created.keys, vec!["~wsk:*", "%R~cfg:*"]);
        assert_eq!(created.selectors.len(), 1);
        assert!(
            created.selectors[0].contains("~cache:*"),
            "{:?}",
            created.selectors
        );

        let mut edit = AclForm::for_user(&created);
        edit.enabled = false;
        edit.commands = "-@all +@read".into();
        run(setuser_review_script(&edit, true).unwrap()).await;
        let edited = list()
            .await
            .into_iter()
            .find(|u| u.name == name)
            .expect("edited");
        assert!(!edited.enabled);
        assert_eq!(edited.password_hashes, created.password_hashes, "kept");
        assert!(
            !edited.commands.iter().any(|c| c == "+set"),
            "{:?}",
            edited.commands
        );
        assert_eq!(edited.selectors.len(), 1);

        run(deluser_script(&name).unwrap()).await;
        assert!(!list().await.iter().any(|u| u.name == name));

        // The page's own read also returns a log grid.
        let log = run(page_script()).await;
        let _ = log_set(&log.rows);
    }

    #[test]
    fn redis_acl_delete_scripts_deluser_but_not_for_default() {
        let script = deluser_script("wsk_app").unwrap();
        assert!(script.contains("ACL DELUSER wsk_app"));
        assert!(deluser_script("default").is_err());
        assert_eq!(page_script(), "ACL LIST\nACL LOG 50");
        for command in parse_script(&page_script()).unwrap() {
            assert_eq!(command_access(&command), CommandAccess::ReadOnly);
        }
    }

    #[test]
    fn redis_acl_log_rows_from_resp2_pairs_and_resp3_maps() {
        let resp2 = serde_json::json!([
            "count",
            1,
            "reason",
            "auth",
            "context",
            "toplevel",
            "object",
            "AUTH",
            "username",
            "nobody",
            "age-seconds",
            "71.679",
            "client-info",
            "id=1 addr=x"
        ])
        .to_string();
        let resp3 = serde_json::json!({"count": 2, "reason": "command", "context": "toplevel",
            "object": "flushall", "username": "wsk_app", "age-seconds": 1.5, "client-info": "id=2"})
        .to_string();
        let set = log_set(&[
            vec![CellValue::I64(0), CellValue::String(resp2)],
            vec![CellValue::I64(1), CellValue::String(resp3)],
        ]);
        assert_eq!(set.rows.len(), 2);
        assert_eq!(set.columns[5].name, "age_seconds");
        assert_eq!(set.rows[0][1], CellValue::String("auth".into()));
        assert_eq!(set.rows[0][4], CellValue::String("nobody".into()));
        assert_eq!(set.rows[1][3], CellValue::String("flushall".into()));
        assert_eq!(set.rows[1][0], CellValue::String("2".into()));
    }
}
