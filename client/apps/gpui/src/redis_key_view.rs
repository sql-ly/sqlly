//! The Redis key view: one key as a real view rather than a query result —
//! a header strip (type, TTL, encoding, memory, length from
//! `SQLLY.KEYINFO`), a paged grid of the value (`SQLLY.HPAGE` / `LPAGE` /
//! `SPAGE` / `ZPAGE` / `XPAGE` / `STRPAGE`, with Load more and "showing N of
//! M"), the value-format picker on the selected value, and per-type edit
//! affordances that each produce ONE exact command — `SET … [KEEPTTL]`,
//! `HSET` / `HDEL`, `LSET` / `LPUSH` / `RPUSH` / `LREM`, `SADD` / `SREM`,
//! `ZADD` / `ZADD XX` / `ZREM`, `JSON.SET` — shown verbatim before Run.
//! A stream key gets Entries / Groups / Consumers / Pending tabs and the
//! stream actions of [`crate::redis_streams`] (`XADD`, `XDEL`, `XACK`,
//! `XCLAIM`, `XAUTOCLAIM`, `XGROUP CREATE` / `DESTROY`, `XTRIM`); `XTRIM`
//! and `XGROUP DESTROY` also ask for the threshold or the group's name to
//! be typed before Run.
//!
//! Bytes are exact end to end. Keys, fields, members, scores and values are
//! `Vec<u8>` from the grid row to the command text (`quote_argument` escapes
//! what the script tokenizer decodes back byte for byte), the editor shows a
//! value as UTF-8 when it is valid and as hex otherwise, and a hex-mode
//! input writes bytes that never pass through a `String`. Nothing here ever
//! parses a value as a number: `00014` stays `00014`.
//!
//! Writes go through `tool_dialog::run_query`, which bypasses the editor's
//! run gate, so the view carries the gate's checks: the connection's
//! read-only policy and a locked production guard (re-read right before
//! every write) refuse writes outright, and the same mutation review the
//! editor applies (`analyze_redis` + the connection's safety features) asks
//! for a second confirming click when it would ask the editor to confirm.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::SqllyDataTable;
use sqlly_intel::query::safety::ProductionDecision;
use sqlly_intel::query::safety::redis::quote_argument;

use crate::dialog::{Dialog, DialogFrame};
use crate::redis_streams::{
    AUTOCLAIM_COUNT, DEFAULT_AUTOCLAIM_IDLE_MS, GroupRow, PENDING_PAGE, StreamAction, StreamTab,
    TrimStrategy,
};
use crate::structured_data::{FORMAT_PICKER_MAX_BYTES, FormatScope, ValueFormat, render_payload};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent, ToolQueryOutcome, ToolQuerySet};

impl EventEmitter<ToolDialogEvent> for RedisKeyViewPane {}

/// Elements one page of a container key loads.
pub const PAGE_SIZE: usize = 1000;
/// Bytes one window of a long string loads (the driver's window).
pub const STRING_WINDOW_BYTES: usize = 1024 * 1024;
/// The detail block renders at most this much of a value; the rest is
/// reported, never silently dropped.
pub const DETAIL_MAX_BYTES: usize = 64 * 1024;
/// A string or JSON document larger than this is not edited in place (the
/// textarea would be the wrong tool); the view says to use a query tab.
pub const EDIT_MAX_BYTES: usize = FORMAT_PICKER_MAX_BYTES;

/// The header strip, as `SQLLY.KEYINFO` answered it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyHeader {
    pub kind: String,
    pub ttl_seconds: Option<i64>,
    pub encoding: Option<String>,
    pub memory_bytes: Option<i64>,
    pub length: Option<i64>,
}

impl KeyHeader {
    /// Read the one-row set by column name.
    pub fn from_set(set: &ToolQuerySet) -> Option<Self> {
        let row = set.rows.first()?;
        let cell = |name: &str| {
            set.columns
                .iter()
                .position(|c| c.name == name)
                .and_then(|i| row.get(i))
        };
        let int = |name: &str| match cell(name) {
            Some(CellValue::I64(n)) => Some(*n),
            _ => None,
        };
        let text = |name: &str| match cell(name) {
            Some(CellValue::String(s)) => Some(s.clone()),
            _ => None,
        };
        Some(KeyHeader {
            kind: text("type").unwrap_or_else(|| "none".to_string()),
            ttl_seconds: int("ttl_seconds"),
            encoding: text("encoding"),
            memory_bytes: int("memory_bytes"),
            length: int("length"),
        })
    }

    /// The strip's cells: label → value.
    pub fn strip(&self) -> Vec<(String, String)> {
        vec![
            (
                crate::localization::text("explorer.redis_key_view.type_label"),
                display_kind(&self.kind),
            ),
            (
                crate::localization::text("explorer.redis_key_view.ttl_label"),
                match self.ttl_seconds {
                    Some(ttl) => crate::localization::format(
                        "explorer.redis_key_view.ttl_value",
                        &[
                            ("duration", &crate::schema_tree::redis::ttl_text(ttl)),
                            ("seconds", &ttl.to_string()),
                        ],
                    ),
                    None => crate::localization::text("explorer.redis_key_view.no_expiry"),
                },
            ),
            (
                crate::localization::text("explorer.redis_key_view.encoding_label"),
                self.encoding.clone().unwrap_or_else(|| "—".to_string()),
            ),
            (
                crate::localization::text("explorer.redis_key_view.memory_label"),
                match self.memory_bytes {
                    Some(bytes) => crate::background_tasks::human_bytes(bytes.max(0) as u64),
                    None => {
                        crate::localization::text("explorer.redis_key_view.memory_not_measured")
                    }
                },
            ),
            (
                crate::localization::text("explorer.redis_key_view.length_label"),
                match self.length {
                    Some(n) => n.to_string(),
                    None => "—".to_string(),
                },
            ),
        ]
    }
}

/// The type names as the strip shows them.
pub fn display_kind(kind: &str) -> String {
    match kind {
        "ReJSON-RL" => "JSON".to_string(),
        "zset" => crate::localization::text("explorer.redis_key_view.sorted_set_kind"),
        other => other.to_string(),
    }
}

/// `true` for the JSON module's type.
pub fn is_json_kind(kind: &str) -> bool {
    matches!(kind, "ReJSON-RL" | "json")
}

/// `SQLLY.KEYINFO <key>`.
pub fn info_command(key: &[u8]) -> String {
    format!("SQLLY.KEYINFO {}", quote_argument(key))
}

/// The paged reader a type calls for, continuing at `next` (the previous
/// page's `next` cell) or from the start. `None` for types with no reader
/// here (module types other than JSON): the header still shows.
pub fn page_command(kind: &str, key: &[u8], next: Option<&str>, count: usize) -> Option<String> {
    let key = quote_argument(key);
    let from = |start: &str| next.unwrap_or(start).to_string();
    Some(match kind {
        "string" => format!("SQLLY.STRPAGE {key} {} {STRING_WINDOW_BYTES}", from("0")),
        "hash" => format!("SQLLY.HPAGE {key} {} {count}", from("0")),
        "list" => format!("SQLLY.LPAGE {key} {} {count}", from("0")),
        "set" => format!("SQLLY.SPAGE {key} {} {count}", from("0")),
        "zset" => format!("SQLLY.ZPAGE {key} {} {count}", from("0")),
        "stream" => format!("SQLLY.XPAGE {key} {} {count}", from("-")),
        kind if is_json_kind(kind) => {
            if next.is_some() {
                return None;
            }
            format!("JSON.GET {key}")
        }
        _ => return None,
    })
}

/// One page as the view keeps it: the data columns, the rows, and the
/// trailer (`next`, `total`) the paged readers append.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Page {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
    pub next: Option<String>,
    /// `None` when the reader carried no total (`JSON.GET`).
    pub total: Option<i64>,
}

/// Split a reader's result set into its page. An empty page (one all-NULL
/// data row) yields no rows but keeps the trailer.
pub fn parse_page(set: &ToolQuerySet) -> Page {
    let n = set.columns.len();
    let trailer = n >= 2 && set.columns[n - 2].name == "next" && set.columns[n - 1].name == "total";
    if !trailer {
        return Page {
            columns: set.columns.clone(),
            rows: set.rows.clone(),
            next: None,
            total: None,
        };
    }
    let data = n - 2;
    let first = set.rows.first();
    let next = match first.and_then(|r| r.get(data)) {
        Some(CellValue::String(s)) => Some(s.clone()),
        _ => None,
    };
    let total = match first.and_then(|r| r.get(data + 1)) {
        Some(CellValue::I64(t)) => Some(*t),
        _ => None,
    };
    let rows = set
        .rows
        .iter()
        .map(|r| r.iter().take(data).cloned().collect::<Vec<_>>())
        .filter(|r| !r.iter().all(|c| *c == CellValue::Null))
        .collect();
    Page {
        columns: set.columns.iter().take(data).cloned().collect(),
        rows,
        next,
        total,
    }
}

/// A grid cell's exact bytes: text as its UTF-8, binary as is. Numbers
/// never come from the readers' value columns, but an `index` column is one
/// and is deliberately not a value.
pub fn cell_bytes(cell: &CellValue) -> Option<Vec<u8>> {
    match cell {
        CellValue::String(s) => Some(s.as_bytes().to_vec()),
        CellValue::Bytes(b) => Some(b.clone()),
        _ => None,
    }
}

/// Hex text (`ff 00 0a`, spaces and newlines ignored, `0x` prefixes
/// allowed) to bytes, byte-exact.
pub fn hex_to_bytes(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut digits = String::new();
    for token in text.split_whitespace() {
        let token = token.strip_prefix("0x").unwrap_or(token);
        digits.push_str(token);
    }
    if !digits.len().is_multiple_of(2) {
        return Err(crate::localization::text(
            "explorer.redis_key_view.hex_even_digits",
        ));
    }
    let bytes = digits.as_bytes();
    for pair in bytes.chunks(2) {
        let text = std::str::from_utf8(pair)
            .map_err(|_| crate::localization::text("explorer.redis_key_view.hex_invalid_digit"))?;
        out.push(u8::from_str_radix(text, 16).map_err(|_| {
            crate::localization::format(
                "explorer.redis_key_view.hex_invalid_digit_in",
                &[("value", &format!("{text:?}"))],
            )
        })?);
    }
    Ok(out)
}

/// Bytes as hex text for the editor (`ff 00 0a`).
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the editor pre-fills for a value: the text when it is valid UTF-8,
/// else its hex with hex mode on.
pub fn editor_text_for(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_string(), false),
        Err(_) => (bytes_to_hex(bytes), true),
    }
}

/// The typed input as bytes: hex mode decodes, text mode takes the UTF-8.
pub fn input_bytes(text: &str, hex: bool) -> Result<Vec<u8>, String> {
    if hex {
        hex_to_bytes(text)
    } else {
        Ok(text.as_bytes().to_vec())
    }
}

/// The decompressed text an edit form starts from, when the value is
/// compressed in a framing that can be packed back exactly: with Auto the
/// value's own bytes decide, with a compressed format picked that format;
/// any other picked format (Hex, UTF-8, …) edits the raw bytes.
pub fn recompress_prefill(
    bytes: &[u8],
    format: ValueFormat,
) -> Option<crate::value_recompress::EditableValue> {
    let format = match format {
        ValueFormat::Auto => crate::structured_data::sniff_format(bytes)?,
        other => other,
    };
    crate::value_recompress::compressed_format(Some(format))?;
    crate::value_recompress::editable(bytes, format).ok()
}

/// One edit the view can run: exactly one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyEdit {
    SetString {
        value: Vec<u8>,
        keep_ttl: bool,
    },
    HashSet {
        field: Vec<u8>,
        value: Vec<u8>,
    },
    HashDel {
        field: Vec<u8>,
    },
    ListSet {
        index: i64,
        value: Vec<u8>,
    },
    ListPush {
        head: bool,
        value: Vec<u8>,
    },
    ListRem {
        value: Vec<u8>,
    },
    SetAdd {
        member: Vec<u8>,
    },
    SetRem {
        member: Vec<u8>,
    },
    ZsetAdd {
        member: Vec<u8>,
        score: Vec<u8>,
        existing_only: bool,
    },
    ZsetRem {
        member: Vec<u8>,
    },
    JsonSet {
        document: Vec<u8>,
    },
    /// A stream action (WS-G).
    Stream(StreamAction),
}

impl KeyEdit {
    /// The exact command line, every argument quoted byte-exact.
    pub fn command(&self, key: &[u8]) -> String {
        let k = quote_argument(key);
        match self {
            Self::SetString { value, keep_ttl } => {
                let mut text = format!("SET {k} {}", quote_argument(value));
                if *keep_ttl {
                    text.push_str(" KEEPTTL");
                }
                text
            }
            Self::HashSet { field, value } => format!(
                "HSET {k} {} {}",
                quote_argument(field),
                quote_argument(value)
            ),
            Self::HashDel { field } => format!("HDEL {k} {}", quote_argument(field)),
            Self::ListSet { index, value } => {
                format!("LSET {k} {index} {}", quote_argument(value))
            }
            Self::ListPush { head: true, value } => {
                format!("LPUSH {k} {}", quote_argument(value))
            }
            Self::ListPush { head: false, value } => {
                format!("RPUSH {k} {}", quote_argument(value))
            }
            Self::ListRem { value } => format!("LREM {k} 1 {}", quote_argument(value)),
            Self::SetAdd { member } => format!("SADD {k} {}", quote_argument(member)),
            Self::SetRem { member } => format!("SREM {k} {}", quote_argument(member)),
            Self::ZsetAdd {
                member,
                score,
                existing_only,
            } => format!(
                "ZADD {k}{} {} {}",
                if *existing_only { " XX" } else { "" },
                quote_argument(score),
                quote_argument(member)
            ),
            Self::ZsetRem { member } => format!("ZREM {k} {}", quote_argument(member)),
            Self::JsonSet { document } => format!("JSON.SET {k} $ {}", quote_argument(document)),
            Self::Stream(action) => action.command(key),
        }
    }

    /// A caveat the review shows beside the command.
    pub fn note(&self) -> Option<String> {
        match self {
            Self::ListRem { .. } => Some(crate::localization::text(
                "explorer.redis_key_view.note_list_remove",
            )),
            Self::SetString {
                keep_ttl: false, ..
            } => Some(crate::localization::text(
                "explorer.redis_key_view.note_keep_ttl",
            )),
            Self::ZsetAdd {
                existing_only: true,
                ..
            } => Some(crate::localization::text(
                "explorer.redis_key_view.note_zadd_existing",
            )),
            Self::Stream(action) => action.note(),
            _ => None,
        }
    }
}

/// Why this view may not write on its connection: the profile's read-only
/// policy or a locked production guard. `None` when writes may proceed
/// (still subject to the review below).
pub fn write_block_for(
    profile: &sqlly_core::ConnectionProfile,
    production: ProductionDecision,
) -> Option<String> {
    crate::redis_bulk::write_block_reason(
        profile.enforce_read_only || profile.block_all_but_select,
        production,
    )
}

/// Whether the editor's run gate would make the user confirm `text` with
/// these safety features (the same Redis analysis and policy).
pub fn confirmation_required(
    features: &crate::mutation_review::SafetyFeatures,
    production: ProductionDecision,
    text: &str,
) -> bool {
    match crate::mutation_review::analyze_redis(text) {
        Some(analysis) => features.requires_confirmation(&analysis, production),
        None => false,
    }
}

/// The edit form that is open.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EditForm {
    StringValue,
    HashAdd,
    HashEdit { field: Vec<u8> },
    HashDelete { field: Vec<u8> },
    ListSet { index: i64 },
    ListPush { head: bool },
    ListRemove { value: Vec<u8> },
    SetAdd,
    SetRemove { member: Vec<u8> },
    ZsetAdd,
    ZsetScore { member: Vec<u8> },
    ZsetRemove { member: Vec<u8> },
    JsonDocument,
    StreamAppend,
    StreamDelete { id: Vec<u8> },
    StreamTrim,
    StreamGroupCreate,
    StreamGroupDestroy { group: Vec<u8> },
    StreamAck { group: Vec<u8>, id: Vec<u8> },
    StreamClaim { group: Vec<u8>, id: Vec<u8> },
    StreamAutoClaim { group: Vec<u8> },
}

impl EditForm {
    fn title(&self) -> String {
        crate::localization::text(match self {
            Self::StringValue => "explorer.redis_key_view.edit_string_title",
            Self::HashAdd => "explorer.redis_key_view.hash_add_title",
            Self::HashEdit { .. } => "explorer.redis_key_view.hash_edit_title",
            Self::HashDelete { .. } => "explorer.redis_key_view.hash_delete_title",
            Self::ListSet { .. } => "explorer.redis_key_view.list_set_title",
            Self::ListPush { head: true } => "explorer.redis_key_view.list_push_head_title",
            Self::ListPush { head: false } => "explorer.redis_key_view.list_push_tail_title",
            Self::ListRemove { .. } => "explorer.redis_key_view.list_remove_title",
            Self::SetAdd => "explorer.redis_key_view.set_add_title",
            Self::SetRemove { .. } => "explorer.redis_key_view.set_remove_title",
            Self::ZsetAdd => "explorer.redis_key_view.zset_add_title",
            Self::ZsetScore { .. } => "explorer.redis_key_view.zset_score_title",
            Self::ZsetRemove { .. } => "explorer.redis_key_view.zset_remove_title",
            Self::JsonDocument => "explorer.redis_key_view.json_edit_title",
            Self::StreamAppend => "explorer.redis_key_view.stream_append_title",
            Self::StreamDelete { .. } => "explorer.redis_key_view.stream_delete_title",
            Self::StreamTrim => "explorer.redis_key_view.stream_trim_title",
            Self::StreamGroupCreate => "explorer.redis_key_view.stream_group_create_title",
            Self::StreamGroupDestroy { .. } => "explorer.redis_key_view.stream_group_destroy_title",
            Self::StreamAck { .. } => "explorer.redis_key_view.stream_ack_title",
            Self::StreamClaim { .. } => "explorer.redis_key_view.stream_claim_title",
            Self::StreamAutoClaim { .. } => "explorer.redis_key_view.stream_auto_claim_title",
        })
    }

    fn wants_value(&self) -> bool {
        matches!(
            self,
            Self::StringValue
                | Self::HashAdd
                | Self::HashEdit { .. }
                | Self::ListSet { .. }
                | Self::ListPush { .. }
                | Self::JsonDocument
                | Self::StreamAppend
        )
    }

    fn wants_field(&self) -> bool {
        matches!(
            self,
            Self::HashAdd | Self::SetAdd | Self::ZsetAdd | Self::StreamAppend
        )
    }

    fn wants_score(&self) -> bool {
        matches!(self, Self::ZsetAdd | Self::ZsetScore { .. })
    }

    fn is_removal(&self) -> bool {
        matches!(
            self,
            Self::HashDelete { .. }
                | Self::ListRemove { .. }
                | Self::SetRemove { .. }
                | Self::ZsetRemove { .. }
                | Self::StreamDelete { .. }
                | Self::StreamTrim
                | Self::StreamGroupDestroy { .. }
        )
    }

    /// Whether the form edits a stream (its inputs are the stream ones).
    fn is_stream(&self) -> bool {
        matches!(
            self,
            Self::StreamAppend
                | Self::StreamDelete { .. }
                | Self::StreamTrim
                | Self::StreamGroupCreate
                | Self::StreamGroupDestroy { .. }
                | Self::StreamAck { .. }
                | Self::StreamClaim { .. }
                | Self::StreamAutoClaim { .. }
        )
    }
}

/// The text inputs, created on the first render (they need the window).
struct Inputs {
    key: Entity<InputState>,
    field: Entity<InputState>,
    score: Entity<InputState>,
    value: Entity<TextareaState>,
    /// Stream forms: an entry id (XADD id, XGROUP CREATE start, XAUTOCLAIM
    /// start), a group, a consumer, a count / idle threshold, and the typed
    /// confirmation XTRIM and XGROUP DESTROY ask for.
    stream_id: Entity<InputState>,
    group: Entity<InputState>,
    consumer: Entity<InputState>,
    number: Entity<InputState>,
    confirm: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

/// What a stream key's tabs hold. Reset when the view opens another key;
/// kept (tab and group) across a refresh.
#[derive(Default)]
struct StreamState {
    tab: StreamTab,
    groups_set: Option<ToolQuerySet>,
    groups: Vec<GroupRow>,
    /// The group the Consumers and Pending tabs show.
    group: Option<Vec<u8>>,
    consumers_set: Option<ToolQuerySet>,
    pending: Page,
    pending_loaded: bool,
    grid: Option<Entity<SqllyDataTable>>,
    selected: Option<usize>,
    _grid_obs: Option<Subscription>,
    loading: bool,
    /// XADD pairs added so far (the form's field/value inputs add one more).
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
    trim_strategy: TrimStrategy,
    trim_approximate: bool,
    generation: u64,
}

pub struct RedisKeyViewPane {
    context: ToolDialogContext,
    /// The key, byte-exact; `None` until the user names one (palette open).
    key: Option<Vec<u8>>,
    header: Option<KeyHeader>,
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<CellValue>>,
    /// A string's loaded windows, concatenated (the editable value).
    string_bytes: Vec<u8>,
    next: Option<String>,
    total: Option<i64>,
    grid: Option<Entity<SqllyDataTable>>,
    selected: Option<usize>,
    format: ValueFormat,
    format_note: Option<String>,
    form: Option<EditForm>,
    hex_mode: bool,
    keep_ttl: bool,
    /// The open form edits a compressed value's decompressed text: the
    /// framing it is packed back into while `recompress_on` is ticked.
    recompress: Option<crate::value_recompress::Framing>,
    recompress_on: bool,
    /// The Protobuf message type picked for this key (custom schemas).
    message_choice: Option<crate::protobuf_schema::MessageChoice>,
    /// The review asked for a confirming click and is waiting for it.
    armed: bool,
    loading: bool,
    running: bool,
    error: Option<String>,
    write_block: Option<String>,
    production: ProductionDecision,
    production_check: crate::tool_dialog::ProductionCheck,
    generation: u64,
    inputs: Option<Inputs>,
    _grid_obs: Option<Subscription>,
    stream: StreamState,
    /// Tests run the write path without the process-wide connection store.
    #[cfg(test)]
    test_safety: Option<crate::mutation_review::SafetyFeatures>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    windowed: bool,
    /// What the popped-out window's title is built from, shared with the
    /// window-title registry so a language switch re-titles it from the
    /// view's current key and connection (kept current by `render`).
    title_inputs: std::rc::Rc<std::cell::RefCell<TitleInputs>>,
    /// The OS title last applied while windowed.
    applied_window_title: Option<String>,
}

/// Language-neutral inputs of [`RedisKeyViewPane::title`].
#[derive(Clone)]
struct TitleInputs {
    key: Option<String>,
    profile: sqlly_core::ConnectionProfile,
}

impl TitleInputs {
    fn title(&self) -> String {
        match &self.key {
            Some(key) => crate::localization::format(
                "explorer.redis_key_view.title_key",
                &[
                    ("key", key),
                    (
                        "database",
                        &crate::schema_tree::redis::database_label_text(
                            self.profile.database.as_deref().unwrap_or("0"),
                            &self.profile,
                        ),
                    ),
                ],
            ),
            None => crate::localization::format(
                "explorer.redis_key_view.title_prompt",
                &[("connection", &self.profile.name)],
            ),
        }
    }
}

impl RedisKeyViewPane {
    /// Open the view on `key` (or on no key, with a name prompt).
    /// `production` is the guard's decision at open time; `production_check`
    /// re-reads it before every write.
    pub fn new(
        context: ToolDialogContext,
        key: Option<Vec<u8>>,
        production: ProductionDecision,
        production_check: crate::tool_dialog::ProductionCheck,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let write_block = write_block_for(&context.profile, production);
        let title_inputs = std::rc::Rc::new(std::cell::RefCell::new(TitleInputs {
            key: None,
            profile: context.profile.clone(),
        }));
        let mut this = Self {
            context,
            key: None,
            header: None,
            columns: Vec::new(),
            rows: Vec::new(),
            string_bytes: Vec::new(),
            next: None,
            total: None,
            grid: None,
            selected: None,
            format: ValueFormat::Auto,
            format_note: None,
            form: None,
            hex_mode: false,
            keep_ttl: true,
            recompress: None,
            recompress_on: true,
            message_choice: None,
            armed: false,
            loading: false,
            running: false,
            error: None,
            write_block,
            production,
            production_check,
            generation: 0,
            inputs: None,
            _grid_obs: None,
            stream: StreamState::default(),
            #[cfg(test)]
            test_safety: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(920.0), Theme::scaled(660.0)),
            windowed: false,
            title_inputs,
            applied_window_title: None,
        };
        if let Some(key) = key {
            this.open_key(key, cx);
        }
        this
    }

    /// Point an open view at another key (or connection): drop the old
    /// value and reload. A load still in flight for the old key is dropped
    /// by the generation bump.
    pub fn retarget(
        &mut self,
        context: ToolDialogContext,
        key: Option<Vec<u8>>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.context = context;
        self.write_block = write_block_for(&self.context.profile, self.production);
        self.form = None;
        self.armed = false;
        self.error = None;
        match key {
            Some(key) => self.open_key(key, cx),
            None => {
                self.key = None;
                self.clear_value();
                self.generation += 1;
                cx.notify();
            }
        }
    }

    fn clear_value(&mut self) {
        self.header = None;
        self.columns.clear();
        self.rows.clear();
        self.string_bytes.clear();
        self.next = None;
        self.total = None;
        self.grid = None;
        self._grid_obs = None;
        self.selected = None;
        self.format_note = None;
        self.stream.generation += 1;
        self.stream.groups_set = None;
        self.stream.groups.clear();
        self.stream.consumers_set = None;
        self.stream.pending = Page::default();
        self.stream.pending_loaded = false;
        self.stream.grid = None;
        self.stream._grid_obs = None;
        self.stream.selected = None;
        self.stream.loading = false;
    }

    fn key_text(&self) -> String {
        self.key
            .as_deref()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .unwrap_or_default()
    }

    fn kind(&self) -> &str {
        self.header.as_ref().map(|h| h.kind.as_str()).unwrap_or("")
    }

    /// Load `key`: the header first, then its first page.
    fn open_key(&mut self, key: Vec<u8>, cx: &mut gpui::Context<Self>) {
        let generation = self.stream.generation + 1;
        self.stream = StreamState {
            generation,
            ..StreamState::default()
        };
        self.key = Some(key);
        self.format = self.remembered_format();
        self.message_choice = self
            .format_identity()
            .and_then(|id| crate::protobuf_schema::remembered_message(&id));
        self.refresh(cx);
    }

    fn format_identity(&self) -> Option<String> {
        let key = self.key.as_deref()?;
        let scope = FormatScope {
            connection_id: self.context.profile.id.0.to_string(),
            database: self.context.profile.database.clone().unwrap_or_default(),
        };
        Some(crate::structured_data::scoped_value_format_identity(
            Some(&scope),
            &format!("GET {}", quote_argument(key)),
            0,
            0,
            "value",
        ))
    }

    fn remembered_format(&self) -> ValueFormat {
        self.format_identity()
            .and_then(|id| crate::structured_data::remembered_format(&id))
            .unwrap_or(ValueFormat::Auto)
    }

    fn set_format(&mut self, format: ValueFormat, cx: &mut gpui::Context<Self>) {
        self.format = format;
        self.format_note = None;
        if let Some(id) = self.format_identity()
            && let Err(error) = crate::structured_data::remember_format(&id, format)
        {
            self.format_note = Some(error);
        }
        cx.notify();
    }

    /// Reload the header and the first page.
    fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(key) = self.key.clone() else {
            return;
        };
        self.clear_value();
        self.loading = true;
        self.error = None;
        self.generation += 1;
        let generation = self.generation;
        crate::tool_dialog::run_query(
            &self.context,
            info_command(&key),
            cx,
            move |this, outcome: ToolQueryOutcome, cx| {
                if this.generation != generation {
                    return;
                }
                if let Some(error) = outcome.error {
                    this.loading = false;
                    this.error = Some(error);
                    cx.notify();
                    return;
                }
                let header = outcome.sets.first().and_then(KeyHeader::from_set);
                match header {
                    Some(header) if header.kind != "none" => {
                        this.header = Some(header);
                        this.load_page(None, cx);
                    }
                    _ => {
                        this.loading = false;
                        this.header = None;
                        this.error = Some(crate::localization::format(
                            "explorer.redis_key_view.key_missing",
                            &[
                                ("key", &this.key_text()),
                                ("database", &this.database_label()),
                            ],
                        ));
                        cx.notify();
                    }
                }
            },
        );
        cx.notify();
    }

    /// Load the page after `next` (or the first), appending its rows.
    fn load_page(&mut self, next: Option<String>, cx: &mut gpui::Context<Self>) {
        let Some(key) = self.key.clone() else {
            return;
        };
        let Some(command) = page_command(self.kind(), &key, next.as_deref(), PAGE_SIZE) else {
            // A type with no reader here: the header strip is the view.
            self.loading = false;
            cx.notify();
            return;
        };
        self.loading = true;
        self.error = None;
        let generation = self.generation;
        let first_page = next.is_none();
        crate::tool_dialog::run_query_with_cap(
            &self.context,
            command,
            PAGE_SIZE + 1,
            cx,
            move |this, outcome: ToolQueryOutcome, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                if let Some(error) = outcome.error {
                    this.error = Some(error);
                    cx.notify();
                    return;
                }
                let Some(set) = outcome.sets.first() else {
                    cx.notify();
                    return;
                };
                let page = parse_page(set);
                if this.kind() == "string" {
                    for row in &page.rows {
                        if let Some(bytes) = row.first().and_then(cell_bytes) {
                            this.string_bytes.extend_from_slice(&bytes);
                        }
                    }
                }
                this.columns = page.columns;
                this.rows.extend(page.rows);
                this.next = page.next;
                this.total = page.total;
                this.rebuild_grid(cx);
                if first_page && this.kind() == "stream" {
                    this.load_stream_groups(cx);
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    fn rebuild_grid(&mut self, cx: &mut gpui::Context<Self>) {
        let set = ToolQuerySet {
            columns: self.columns.clone(),
            rows: self.rows.clone(),
        };
        self.grid = crate::tool_dialog::build_result_grid(&set, cx);
        self.selected = None;
        self._grid_obs = self.grid.as_ref().map(|grid| {
            let state = grid.read(cx).state.clone();
            cx.observe(&state, |this, _state, cx| {
                let selected = this
                    .grid
                    .as_ref()
                    .and_then(|grid| crate::tool_dialog::selected_source_row(grid, cx));
                if selected != this.selected {
                    this.selected = selected;
                    cx.notify();
                }
            })
        });
    }

    /// The selected row, if any.
    fn selected_row(&self) -> Option<&Vec<CellValue>> {
        self.rows.get(self.selected?)
    }

    /// The bytes the detail block shows: the string's loaded value, the
    /// JSON document, or the selected row's value.
    fn detail_bytes(&self) -> Option<Vec<u8>> {
        let kind = self.kind();
        if kind == "string" {
            return (!self.rows.is_empty()).then(|| self.string_bytes.clone());
        }
        if is_json_kind(kind) {
            return self
                .rows
                .first()
                .and_then(|r| r.first())
                .and_then(cell_bytes);
        }
        let row = self.selected_row()?;
        let column = match kind {
            "hash" | "list" | "stream" | "zset" => 1,
            _ => 0,
        };
        row.get(column).and_then(cell_bytes)
    }

    /// The "showing N of M" line.
    fn coverage_text(&self) -> String {
        let kind = self.kind();
        if kind == "string" {
            let loaded = self.string_bytes.len();
            return match self.total {
                Some(total) if total as usize > loaded => crate::localization::format(
                    "explorer.redis_key_view.string_coverage_partial",
                    &[
                        (
                            "loaded",
                            &crate::background_tasks::human_bytes(loaded as u64),
                        ),
                        (
                            "total",
                            &crate::background_tasks::human_bytes(total.max(0) as u64),
                        ),
                    ],
                ),
                _ => crate::localization::format(
                    "explorer.redis_key_view.string_coverage",
                    &[(
                        "loaded",
                        &crate::background_tasks::human_bytes(loaded as u64),
                    )],
                ),
            };
        }
        if is_json_kind(kind) {
            return crate::localization::text("explorer.redis_key_view.json_coverage");
        }
        match self.total {
            Some(total) => crate::localization::format(
                "explorer.redis_key_view.collection_coverage",
                &[
                    ("shown", &self.rows.len().to_string()),
                    ("total", &total.to_string()),
                    ("element", &element_name(kind)),
                ],
            ),
            None => crate::localization::format(
                "explorer.redis_key_view.collection_count",
                &[
                    ("count", &self.rows.len().to_string()),
                    ("element", &element_name(kind)),
                ],
            ),
        }
    }

    // ----- editing -------------------------------------------------------

    fn ensure_inputs(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.inputs.is_some() {
            return;
        }
        let key = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.redis_key_view.key_placeholder",
            ))
        });
        let field = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.redis_key_view.field_placeholder",
            ))
        });
        let score = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.redis_key_view.score_placeholder",
            ))
        });
        let value = cx.new(|cx| {
            TextareaState::new(window, cx)
                .soft_wrap(true)
                .auto_grow(3, 10)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.redis_key_view.value_placeholder",
                ))
        });
        let stream_id = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.redis_key_view.stream_id_placeholder",
            ))
        });
        let group = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.redis_key_view.group_placeholder",
            ))
        });
        let consumer = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.redis_key_view.consumer_placeholder",
            ))
        });
        let number = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.redis_key_view.count_placeholder",
            ))
        });
        let confirm = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.redis_key_view.confirm_placeholder",
            ))
        });
        let mut subscriptions = Vec::new();
        for input in [
            &key, &field, &score, &stream_id, &group, &consumer, &number, &confirm,
        ] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this: &mut Self, _state, event: &InputEvent, _window, cx| match event {
                    InputEvent::Change => {
                        this.armed = false;
                        this.error = None;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } if this.key.is_none() => {
                        this.open_typed_key(cx);
                    }
                    _ => {}
                },
            ));
        }
        subscriptions.push(cx.subscribe_in(
            &value,
            window,
            |this: &mut Self, _state, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.armed = false;
                    this.error = None;
                    cx.notify();
                }
            },
        ));
        self.inputs = Some(Inputs {
            key,
            field,
            score,
            value,
            stream_id,
            group,
            consumer,
            number,
            confirm,
            _subscriptions: subscriptions,
        });
    }

    fn open_typed_key(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(inputs) = &self.inputs else {
            return;
        };
        let typed = inputs.key.read(cx).value().to_string();
        if typed.is_empty() {
            return;
        }
        self.open_key(typed.into_bytes(), cx);
    }

    fn field_text(&self, cx: &gpui::App) -> String {
        self.inputs
            .as_ref()
            .map(|i| i.field.read(cx).value().to_string())
            .unwrap_or_default()
    }

    fn score_text(&self, cx: &gpui::App) -> String {
        self.inputs
            .as_ref()
            .map(|i| i.score.read(cx).value().to_string())
            .unwrap_or_default()
    }

    fn value_text(&self, cx: &gpui::App) -> String {
        self.inputs
            .as_ref()
            .map(|i| i.value.read(cx).value().to_string())
            .unwrap_or_default()
    }

    fn set_input(
        input: &Entity<InputState>,
        text: String,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        input.update(cx, |state, cx| state.set_value(text, window, cx));
    }

    /// Open an edit form, pre-filling the inputs from the value it edits.
    fn open_form(&mut self, form: EditForm, window: &mut Window, cx: &mut gpui::Context<Self>) {
        self.ensure_inputs(window, cx);
        self.armed = false;
        self.error = None;
        let prefill: Option<Vec<u8>> = match &form {
            EditForm::StringValue => Some(self.string_bytes.clone()),
            EditForm::JsonDocument => self.detail_bytes(),
            EditForm::HashEdit { .. } | EditForm::ListSet { .. } => self
                .selected_row()
                .and_then(|r| r.get(1))
                .and_then(cell_bytes),
            EditForm::ZsetScore { .. } => self
                .selected_row()
                .and_then(|r| r.get(2))
                .and_then(cell_bytes),
            _ => None,
        };
        let Some(inputs) = &self.inputs else {
            return;
        };
        let (field, score, value) = (
            inputs.field.clone(),
            inputs.score.clone(),
            inputs.value.clone(),
        );
        Self::set_input(&field, String::new(), window, cx);
        if matches!(form, EditForm::ZsetScore { .. }) {
            let text = prefill
                .as_deref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            Self::set_input(&score, text, window, cx);
            self.hex_mode = false;
        } else {
            Self::set_input(&score, String::new(), window, cx);
            // A compressed value is edited as its decompressed text and
            // written back packed the same way (while the box is ticked).
            let opened = match &form {
                EditForm::StringValue | EditForm::HashEdit { .. } | EditForm::ListSet { .. } => {
                    prefill
                        .as_deref()
                        .and_then(|bytes| recompress_prefill(bytes, self.format))
                }
                _ => None,
            };
            self.recompress = opened.as_ref().map(|value| value.framing);
            self.recompress_on = true;
            let (text, hex) = match opened {
                Some(value) => (value.text, false),
                None => prefill
                    .as_deref()
                    .map(editor_text_for)
                    .unwrap_or((String::new(), false)),
            };
            value.update(cx, |state, cx| state.set_value(text, window, cx));
            self.hex_mode = hex;
        }
        if form.is_stream() {
            self.prefill_stream_form(&form, window, cx);
        }
        self.keep_ttl = self
            .header
            .as_ref()
            .is_some_and(|h| h.ttl_seconds.is_some());
        self.form = Some(form);
        cx.notify();
    }

    /// Reset the stream inputs for `form` and fill in what it suggests.
    fn prefill_stream_form(
        &mut self,
        form: &EditForm,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(inputs) = &self.inputs else {
            return;
        };
        let (stream_id, group, consumer, number, confirm) = (
            inputs.stream_id.clone(),
            inputs.group.clone(),
            inputs.consumer.clone(),
            inputs.number.clone(),
            inputs.confirm.clone(),
        );
        let selected_idle = self
            .stream_pending_selected()
            .and_then(|row| row.idle_ms)
            .map(|ms| ms.to_string());
        let (id_text, number_text) = match form {
            EditForm::StreamAppend => ("*".to_string(), String::new()),
            EditForm::StreamGroupCreate => ("$".to_string(), String::new()),
            EditForm::StreamAutoClaim { .. } => {
                ("0-0".to_string(), DEFAULT_AUTOCLAIM_IDLE_MS.to_string())
            }
            EditForm::StreamClaim { .. } => (String::new(), selected_idle.unwrap_or_default()),
            EditForm::StreamTrim => (String::new(), String::new()),
            _ => (String::new(), String::new()),
        };
        Self::set_input(&stream_id, id_text, window, cx);
        Self::set_input(&number, number_text, window, cx);
        Self::set_input(&group, String::new(), window, cx);
        Self::set_input(&consumer, String::new(), window, cx);
        Self::set_input(&confirm, String::new(), window, cx);
        if matches!(form, EditForm::StreamAppend) {
            self.stream.pairs.clear();
        }
    }

    fn input_text(&self, pick: impl Fn(&Inputs) -> &Entity<InputState>, cx: &gpui::App) -> String {
        self.inputs
            .as_ref()
            .map(|i| pick(i).read(cx).value().to_string())
            .unwrap_or_default()
    }

    /// The stream action the open stream form describes.
    fn build_stream_action(&self, form: &EditForm, cx: &gpui::App) -> Result<StreamAction, String> {
        use crate::redis_streams::{digits, stream_id};
        let id_text = self.input_text(|i| &i.stream_id, cx);
        let number = self.input_text(|i| &i.number, cx);
        let named = |text: String, key: &'static str| {
            if text.is_empty() {
                Err(crate::localization::text(key))
            } else {
                Ok(text.into_bytes())
            }
        };
        let consumer = || {
            named(
                self.input_text(|i| &i.consumer, cx),
                "explorer.redis_key_view.consumer_required",
            )
        };
        Ok(match form.clone() {
            EditForm::StreamAppend => {
                let mut pairs = self.stream.pairs.clone();
                let field = self.field_text(cx);
                if !field.is_empty() {
                    pairs.push((
                        field.into_bytes(),
                        input_bytes(&self.value_text(cx), self.hex_mode)?,
                    ));
                }
                if pairs.is_empty() {
                    return Err(crate::localization::text(
                        "explorer.redis_key_view.stream_pair_required",
                    ));
                }
                StreamAction::Append {
                    id: stream_id(&id_text, &["*"])?,
                    pairs,
                }
            }
            EditForm::StreamDelete { id } => StreamAction::Delete { id },
            EditForm::StreamTrim => {
                let threshold = match self.stream.trim_strategy {
                    TrimStrategy::MaxLen => digits(
                        &number,
                        &crate::localization::text("explorer.redis_key_view.count_to_keep_field"),
                    )?
                    .into_bytes(),
                    TrimStrategy::MinId => stream_id(&number, &[])?,
                };
                StreamAction::Trim {
                    strategy: self.stream.trim_strategy,
                    approximate: self.stream.trim_approximate,
                    threshold,
                }
            }
            EditForm::StreamGroupCreate => StreamAction::GroupCreate {
                group: named(
                    self.input_text(|i| &i.group, cx),
                    "explorer.redis_key_view.group_required",
                )?,
                start: stream_id(&id_text, &["$", "0"])?,
            },
            EditForm::StreamGroupDestroy { group } => StreamAction::GroupDestroy { group },
            EditForm::StreamAck { group, id } => StreamAction::Ack { group, id },
            EditForm::StreamClaim { group, id } => StreamAction::Claim {
                group,
                consumer: consumer()?,
                min_idle_ms: digits(
                    &number,
                    &crate::localization::text("explorer.redis_key_view.minimum_idle_field"),
                )?,
                id,
            },
            EditForm::StreamAutoClaim { group } => StreamAction::AutoClaim {
                group,
                consumer: consumer()?,
                min_idle_ms: digits(
                    &number,
                    &crate::localization::text("explorer.redis_key_view.minimum_idle_field"),
                )?,
                start: stream_id(&id_text, &["0", "0-0"])?,
                count: AUTOCLAIM_COUNT,
            },
            _ => {
                return Err(crate::localization::text(
                    "explorer.redis_key_view.not_stream_edit",
                ));
            }
        })
    }

    /// Whether the open form's typed confirmation (if it asks for one) is
    /// satisfied.
    fn typed_confirmation_ok(&self, edit: &KeyEdit, cx: &gpui::App) -> bool {
        match edit {
            KeyEdit::Stream(action) => match action.typed_confirmation() {
                Some(expected) => crate::redis_streams::typed_confirmation_matches(
                    &expected,
                    &self.input_text(|i| &i.confirm, cx),
                ),
                None => true,
            },
            _ => true,
        }
    }

    fn close_form(&mut self, cx: &mut gpui::Context<Self>) {
        self.form = None;
        self.armed = false;
        cx.notify();
    }

    /// The edit the open form describes, from the current inputs.
    fn build_edit(&self, cx: &gpui::App) -> Result<KeyEdit, String> {
        let form = self
            .form
            .clone()
            .ok_or_else(|| crate::localization::text("explorer.redis_key_view.no_edit_open"))?;
        let value = || match self.recompress {
            Some(framing) if self.recompress_on && !self.hex_mode => {
                crate::value_recompress::recompress(framing, &self.value_text(cx))
            }
            _ => input_bytes(&self.value_text(cx), self.hex_mode),
        };
        let field = || {
            let text = self.field_text(cx);
            if text.is_empty() {
                Err(crate::localization::text(
                    "explorer.redis_key_view.field_required",
                ))
            } else {
                Ok(text.into_bytes())
            }
        };
        let score = || {
            let text = self.score_text(cx).trim().to_string();
            if text.is_empty() {
                Err(crate::localization::text(
                    "explorer.redis_key_view.score_required",
                ))
            } else {
                Ok(text.into_bytes())
            }
        };
        Ok(match form {
            EditForm::StringValue => KeyEdit::SetString {
                value: value()?,
                keep_ttl: self.keep_ttl,
            },
            EditForm::HashAdd => KeyEdit::HashSet {
                field: field()?,
                value: value()?,
            },
            EditForm::HashEdit { field } => KeyEdit::HashSet {
                field,
                value: value()?,
            },
            EditForm::HashDelete { field } => KeyEdit::HashDel { field },
            EditForm::ListSet { index } => KeyEdit::ListSet {
                index,
                value: value()?,
            },
            EditForm::ListPush { head } => KeyEdit::ListPush {
                head,
                value: value()?,
            },
            EditForm::ListRemove { value } => KeyEdit::ListRem { value },
            EditForm::SetAdd => KeyEdit::SetAdd { member: field()? },
            EditForm::SetRemove { member } => KeyEdit::SetRem { member },
            EditForm::ZsetAdd => KeyEdit::ZsetAdd {
                member: field()?,
                score: score()?,
                existing_only: false,
            },
            EditForm::ZsetScore { member } => KeyEdit::ZsetAdd {
                member,
                score: score()?,
                existing_only: true,
            },
            EditForm::ZsetRemove { member } => KeyEdit::ZsetRem { member },
            EditForm::JsonDocument => KeyEdit::JsonSet { document: value()? },
            stream => KeyEdit::Stream(self.build_stream_action(&stream, cx)?),
        })
    }

    /// Run the open form's command: refuse on a read-only or locked
    /// connection, ask for a confirming click when the editor's gate would,
    /// then run and reload.
    fn run_edit(&mut self, cx: &mut gpui::Context<Self>) {
        if self.running {
            return;
        }
        // The production unlock may have expired since the view opened.
        self.production = (self.production_check)(cx);
        self.write_block = write_block_for(&self.context.profile, self.production);
        if let Some(reason) = &self.write_block {
            self.error = Some(reason.clone());
            cx.notify();
            return;
        }
        let Some(key) = self.key.clone() else {
            return;
        };
        let edit = match self.build_edit(cx) {
            Ok(edit) => edit,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        if !self.typed_confirmation_ok(&edit, cx) {
            self.error = Some(crate::localization::text(
                "explorer.redis_key_view.confirmation_required",
            ));
            cx.notify();
            return;
        }
        let text = edit.command(&key);
        let features = self.safety_features();
        if !self.armed && confirmation_required(&features, self.production, &text) {
            self.armed = true;
            cx.notify();
            return;
        }
        self.armed = false;
        self.running = true;
        self.error = None;
        let describe = text.clone();
        crate::tool_dialog::run_query(
            &self.context,
            text,
            cx,
            move |this, outcome: ToolQueryOutcome, cx| {
                this.running = false;
                if let Some(error) = outcome.error {
                    this.error = Some(error);
                    cx.notify();
                    return;
                }
                let reply = outcome
                    .sets
                    .first()
                    .and_then(|set| set.rows.first())
                    .and_then(|row| row.first())
                    .map(|cell| match cell {
                        CellValue::I64(n) => n.to_string(),
                        CellValue::String(s) => s.clone(),
                        CellValue::Null => "(nil)".to_string(),
                        other => format!("{other:?}"),
                    })
                    .unwrap_or_default();
                cx.emit(ToolDialogEvent::Status {
                    level: crate::app::StatusLevel::Success,
                    message: format!("{describe} → {reply}"),
                });
                this.form = None;
                this.refresh(cx);
            },
        );
        cx.notify();
    }

    /// The connection's safety features, as the editor's run gate reads them.
    fn safety_features(&self) -> crate::mutation_review::SafetyFeatures {
        #[cfg(test)]
        if let Some(features) = &self.test_safety {
            return features.clone();
        }
        crate::mutation_review::SafetyFeatures::effective_for_profile(
            &self.context.profile,
            crate::connection::ConnectionStore::global(),
        )
    }

    fn close(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.windowed {
            window.remove_window();
        } else {
            cx.emit(ToolDialogEvent::Close);
        }
    }

    /// Pop out into a standalone OS window; the inline card closes and this
    /// same view carries on in the window.
    fn pop_out(&mut self, cx: &mut gpui::Context<Self>) {
        let entity = cx.entity();
        self.sync_title_inputs();
        let inputs = self.title_inputs.clone();
        let title = move || inputs.borrow().title();
        cx.defer(move |cx| {
            let build_entity = entity.clone();
            let opened = crate::dialog::open_detached(
                crate::dialog::DetachedOptions::localized_with(
                    title,
                    Theme::scaled(920.0),
                    Theme::scaled(700.0),
                ),
                None,
                cx,
                move |_, cx| {
                    build_entity.update(cx, |pane, _| pane.windowed = true);
                    build_entity
                },
            );
            if opened.is_some() {
                entity.update(cx, |_, cx| cx.emit(ToolDialogEvent::Close));
            }
        });
    }

    fn title(&self) -> String {
        self.current_title_inputs().title()
    }

    fn current_title_inputs(&self) -> TitleInputs {
        TitleInputs {
            key: self.key.as_ref().map(|_| self.key_text()),
            profile: self.context.profile.clone(),
        }
    }

    /// Keep the shared title inputs on the current key/connection.
    fn sync_title_inputs(&mut self) {
        *self.title_inputs.borrow_mut() = self.current_title_inputs();
    }

    /// The view's logical database as the profile names it (`db3 — sessions`).
    fn database_label(&self) -> String {
        crate::schema_tree::redis::database_label_text(
            self.context.profile.database.as_deref().unwrap_or("0"),
            &self.context.profile,
        )
    }
}

// ----- stream tabs ---------------------------------------------------------

impl RedisKeyViewPane {
    /// Read the stream's groups, then the chosen group's consumers and
    /// pending entries.
    fn load_stream_groups(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(key) = self.key.clone() else {
            return;
        };
        self.stream.loading = true;
        let generation = self.stream.generation;
        crate::tool_dialog::run_query(
            &self.context,
            crate::redis_streams::groups_command(&key),
            cx,
            move |this, outcome: ToolQueryOutcome, cx| {
                if this.stream.generation != generation {
                    return;
                }
                this.stream.loading = false;
                if let Some(error) = outcome.error {
                    this.error = Some(error);
                    cx.notify();
                    return;
                }
                let set = outcome.sets.into_iter().next().unwrap_or_default();
                this.stream.groups = crate::redis_streams::parse_groups(&set);
                this.stream.groups_set = Some(set);
                let keep = this
                    .stream
                    .group
                    .as_ref()
                    .is_some_and(|g| this.stream.groups.iter().any(|row| &row.name == g));
                if !keep {
                    this.stream.group = this.stream.groups.first().map(|row| row.name.clone());
                }
                this.load_group_detail(cx);
                this.rebuild_stream_grid(cx);
                cx.notify();
            },
        );
    }

    /// Read the chosen group's consumers and its first pending page.
    fn load_group_detail(&mut self, cx: &mut gpui::Context<Self>) {
        self.stream.consumers_set = None;
        self.stream.pending = Page::default();
        self.stream.pending_loaded = false;
        let (Some(key), Some(group)) = (self.key.clone(), self.stream.group.clone()) else {
            return;
        };
        let generation = self.stream.generation;
        let wanted = group.clone();
        crate::tool_dialog::run_query(
            &self.context,
            crate::redis_streams::consumers_command(&key, &group),
            cx,
            move |this, outcome: ToolQueryOutcome, cx| {
                if this.stream.generation != generation
                    || this.stream.group.as_ref() != Some(&wanted)
                {
                    return;
                }
                if let Some(error) = outcome.error {
                    this.error = Some(error);
                } else {
                    this.stream.consumers_set =
                        Some(outcome.sets.into_iter().next().unwrap_or_default());
                }
                if this.stream.tab == StreamTab::Consumers {
                    this.rebuild_stream_grid(cx);
                }
                cx.notify();
            },
        );
        self.load_pending(None, cx);
    }

    /// Load the pending page after `next` (or the first), appending.
    fn load_pending(&mut self, next: Option<String>, cx: &mut gpui::Context<Self>) {
        let (Some(key), Some(group)) = (self.key.clone(), self.stream.group.clone()) else {
            return;
        };
        self.stream.loading = true;
        let generation = self.stream.generation;
        let wanted = group.clone();
        crate::tool_dialog::run_query_with_cap(
            &self.context,
            crate::redis_streams::pending_command(&key, &group, next.as_deref(), PENDING_PAGE),
            PENDING_PAGE + 1,
            cx,
            move |this, outcome: ToolQueryOutcome, cx| {
                if this.stream.generation != generation
                    || this.stream.group.as_ref() != Some(&wanted)
                {
                    return;
                }
                this.stream.loading = false;
                if let Some(error) = outcome.error {
                    this.error = Some(error);
                    cx.notify();
                    return;
                }
                if let Some(set) = outcome.sets.first() {
                    let page = parse_page(set);
                    this.stream.pending.columns = page.columns;
                    this.stream.pending.rows.extend(page.rows);
                    this.stream.pending.next = page.next;
                    this.stream.pending.total = page.total;
                }
                this.stream.pending_loaded = true;
                if this.stream.tab == StreamTab::Pending {
                    this.rebuild_stream_grid(cx);
                }
                cx.notify();
            },
        );
    }

    fn on_pending_tab(&self) -> bool {
        self.kind() == "stream" && self.stream.tab == StreamTab::Pending
    }

    fn set_stream_tab(&mut self, tab: StreamTab, cx: &mut gpui::Context<Self>) {
        if self.stream.tab == tab {
            return;
        }
        self.stream.tab = tab;
        self.form = None;
        self.armed = false;
        self.rebuild_stream_grid(cx);
        cx.notify();
    }

    fn select_group(&mut self, group: Vec<u8>, cx: &mut gpui::Context<Self>) {
        if self.stream.group.as_ref() == Some(&group) {
            return;
        }
        self.stream.group = Some(group);
        self.form = None;
        self.armed = false;
        self.load_group_detail(cx);
        if self.stream.tab != StreamTab::Groups {
            self.rebuild_stream_grid(cx);
        }
        cx.notify();
    }

    /// The set the current (non-Entries) tab shows.
    fn stream_tab_set(&self) -> Option<ToolQuerySet> {
        match self.stream.tab {
            StreamTab::Entries => None,
            StreamTab::Groups => self.stream.groups_set.clone(),
            StreamTab::Consumers => self.stream.consumers_set.clone(),
            StreamTab::Pending => self.stream.pending_loaded.then(|| ToolQuerySet {
                columns: self.stream.pending.columns.clone(),
                rows: self.stream.pending.rows.clone(),
            }),
        }
    }

    fn rebuild_stream_grid(&mut self, cx: &mut gpui::Context<Self>) {
        self.stream.selected = None;
        self.stream._grid_obs = None;
        self.stream.grid = self
            .stream_tab_set()
            .filter(|set| !set.rows.is_empty())
            .and_then(|set| crate::tool_dialog::build_result_grid(&set, cx));
        self.stream._grid_obs = self.stream.grid.as_ref().map(|grid| {
            let state = grid.read(cx).state.clone();
            cx.observe(&state, |this, _state, cx| {
                let selected = this
                    .stream
                    .grid
                    .as_ref()
                    .and_then(|grid| crate::tool_dialog::selected_source_row(grid, cx));
                if selected == this.stream.selected {
                    return;
                }
                this.stream.selected = selected;
                if this.stream.tab == StreamTab::Groups
                    && let Some(group) = selected.and_then(|i| this.stream.groups.get(i))
                {
                    let name = group.name.clone();
                    this.select_group(name, cx);
                }
                cx.notify();
            })
        });
    }

    /// The selected entry's id on the Entries tab.
    fn stream_entry_selected(&self) -> Option<Vec<u8>> {
        self.selected_row()
            .and_then(|r| r.first())
            .and_then(cell_bytes)
    }

    /// The selected pending entry on the Pending tab.
    fn stream_pending_selected(&self) -> Option<crate::redis_streams::PendingRow> {
        if self.stream.tab != StreamTab::Pending {
            return None;
        }
        let row = self.stream.pending.rows.get(self.stream.selected?)?;
        crate::redis_streams::pending_row(&self.stream.pending.columns, row)
    }

    /// The selected group on the Groups tab.
    fn stream_group_selected(&self) -> Option<Vec<u8>> {
        if self.stream.tab != StreamTab::Groups {
            return None;
        }
        self.stream
            .groups
            .get(self.stream.selected?)
            .map(|g| g.name.clone())
    }

    /// The XADD form's "Add pair": keep the typed field and value and clear
    /// the inputs for the next one.
    fn add_stream_pair(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let field = self.field_text(cx);
        if field.is_empty() {
            self.error = Some(crate::localization::text(
                "explorer.redis_key_view.stream_field_required",
            ));
            cx.notify();
            return;
        }
        let value = match input_bytes(&self.value_text(cx), self.hex_mode) {
            Ok(value) => value,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.stream.pairs.push((field.into_bytes(), value));
        if let Some(inputs) = &self.inputs {
            let (field, value) = (inputs.field.clone(), inputs.value.clone());
            Self::set_input(&field, String::new(), window, cx);
            value.update(cx, |state, cx| state.set_value(String::new(), window, cx));
        }
        self.armed = false;
        cx.notify();
    }

    fn stream_coverage_text(&self) -> String {
        let group = || {
            self.stream
                .group
                .as_deref()
                .map(|g| editor_text_for(g).0)
                .unwrap_or_default()
        };
        match self.stream.tab {
            StreamTab::Entries => self.coverage_text(),
            StreamTab::Groups => crate::localization::format(
                "explorer.redis_key_view.stream_group_count",
                &[("count", &self.stream.groups.len().to_string())],
            ),
            StreamTab::Consumers => {
                let count = self
                    .stream
                    .consumers_set
                    .as_ref()
                    .map_or(0, |s| s.rows.len());
                crate::localization::format(
                    "explorer.redis_key_view.stream_consumer_count",
                    &[("count", &count.to_string()), ("group", &group())],
                )
            }
            StreamTab::Pending => crate::localization::format(
                "explorer.redis_key_view.stream_pending_coverage",
                &[
                    ("shown", &self.stream.pending.rows.len().to_string()),
                    ("total", &self.stream.pending.total.unwrap_or(0).to_string()),
                    ("group", &group()),
                ],
            ),
        }
    }

    fn chip(
        &self,
        id: SharedString,
        label: String,
        active: bool,
        on_click: impl Fn(&mut Self, &mut gpui::Context<Self>) + 'static,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        div()
            .id(id)
            .cursor_pointer()
            .px(px(s(8.0)))
            .py(px(s(3.0)))
            .rounded(px(s(4.0)))
            .text_size(px(Theme::font_size_ratio(0.88)))
            .when(active, |d| {
                d.bg(Theme::accent().opacity(0.2))
                    .text_color(Theme::accent())
            })
            .when(!active, |d| {
                d.text_color(Theme::text_dim())
                    .hover(|st| st.bg(Theme::row_hover()))
            })
            .child(label)
            .on_click(cx.listener(move |this: &mut Self, _event, _window, cx| on_click(this, cx)))
            .into_any_element()
    }

    /// The tab strip (and, on the group tabs, the group chips).
    fn render_stream_tabs(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let mut tabs = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(2.0)))
            .p(px(s(2.0)))
            .rounded(px(s(5.0)))
            .border_1()
            .border_color(Theme::border());
        for tab in StreamTab::ALL {
            tabs = tabs.child(self.chip(
                SharedString::from(format!("redis-stream-tab-{tab:?}")),
                tab.label(),
                self.stream.tab == tab,
                move |this, cx| this.set_stream_tab(tab, cx),
                cx,
            ));
        }
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(8.0)))
            .child(tabs);
        if self.stream.tab.needs_group() {
            if self.stream.groups.is_empty() {
                row = row.child(note_text(crate::localization::text(
                    "explorer.redis_key_view.no_stream_groups_hint",
                )));
            } else {
                row = row.child(note_text(crate::localization::text(
                    "explorer.redis_key_view.stream_group_label",
                )));
                for (i, group) in self.stream.groups.iter().enumerate() {
                    let name = group.name.clone();
                    row = row.child(self.chip(
                        SharedString::from(format!("redis-stream-group-{i}")),
                        editor_text_for(&group.name).0,
                        self.stream.group.as_ref() == Some(&group.name),
                        move |this, cx| this.select_group(name.clone(), cx),
                        cx,
                    ));
                }
            }
        }
        row.into_any_element()
    }

    /// The body of a non-Entries tab.
    fn render_stream_tab_body(&self) -> gpui::AnyElement {
        if let Some(grid) = &self.stream.grid {
            return div()
                .size_full()
                .child(grid.clone().into_any_element())
                .into_any_element();
        }
        let text = if self.stream.loading {
            crate::localization::text("explorer.redis_key_view.stream_loading")
        } else {
            crate::localization::text(match self.stream.tab {
                StreamTab::Groups => "explorer.redis_key_view.stream_groups_empty",
                StreamTab::Consumers | StreamTab::Pending if self.stream.group.is_none() => {
                    "explorer.redis_key_view.stream_group_unselected"
                }
                StreamTab::Consumers => "explorer.redis_key_view.stream_consumers_empty",
                StreamTab::Pending => "explorer.redis_key_view.stream_pending_empty",
                StreamTab::Entries => "explorer.redis_key_view.stream_entries_empty",
            })
        };
        div()
            .text_size(px(Theme::font_size_ratio(0.92)))
            .text_color(Theme::text_dim())
            .child(text)
            .into_any_element()
    }

    /// The stream actions the current tab offers.
    fn stream_actions(
        &self,
        can_write: bool,
        cx: &mut gpui::Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut children = Vec::new();
        match self.stream.tab {
            StreamTab::Entries => {
                children.push(self.action_button(
                    "redis-stream-append",
                    crate::localization::text("explorer.redis_key_view.stream_append_button"),
                    can_write,
                    |_| Some(EditForm::StreamAppend),
                    cx,
                ));
                children.push(self.action_button(
                    "redis-stream-delete",
                    crate::localization::text("explorer.redis_key_view.stream_delete_button"),
                    can_write && self.stream_entry_selected().is_some(),
                    |this| {
                        this.stream_entry_selected()
                            .map(|id| EditForm::StreamDelete { id })
                    },
                    cx,
                ));
                children.push(self.action_button(
                    "redis-stream-trim",
                    crate::localization::text("explorer.redis_key_view.stream_trim_button"),
                    can_write,
                    |_| Some(EditForm::StreamTrim),
                    cx,
                ));
            }
            StreamTab::Groups => {
                children.push(self.action_button(
                    "redis-stream-group-create",
                    crate::localization::text("explorer.redis_key_view.stream_group_create_button"),
                    can_write,
                    |_| Some(EditForm::StreamGroupCreate),
                    cx,
                ));
                children.push(self.action_button(
                    "redis-stream-group-destroy",
                    crate::localization::text(
                        "explorer.redis_key_view.stream_group_destroy_button",
                    ),
                    can_write && self.stream_group_selected().is_some(),
                    |this| {
                        this.stream_group_selected()
                            .map(|group| EditForm::StreamGroupDestroy { group })
                    },
                    cx,
                ));
            }
            StreamTab::Consumers => {
                children.push(note_text(crate::localization::text(
                    "explorer.redis_key_view.stream_consumer_explanation",
                )));
            }
            StreamTab::Pending => {
                let selected = self.stream_pending_selected().is_some();
                let has_group = self.stream.group.is_some();
                children.push(self.action_button(
                    "redis-stream-ack",
                    crate::localization::text("explorer.redis_key_view.stream_ack_button"),
                    can_write && selected,
                    |this| {
                        let row = this.stream_pending_selected()?;
                        Some(EditForm::StreamAck {
                            group: this.stream.group.clone()?,
                            id: row.id,
                        })
                    },
                    cx,
                ));
                children.push(self.action_button(
                    "redis-stream-claim",
                    crate::localization::text("explorer.redis_key_view.stream_claim_button"),
                    can_write && selected,
                    |this| {
                        let row = this.stream_pending_selected()?;
                        Some(EditForm::StreamClaim {
                            group: this.stream.group.clone()?,
                            id: row.id,
                        })
                    },
                    cx,
                ));
                children.push(self.action_button(
                    "redis-stream-autoclaim",
                    crate::localization::text("explorer.redis_key_view.stream_auto_claim_button"),
                    can_write && has_group,
                    |this| {
                        Some(EditForm::StreamAutoClaim {
                            group: this.stream.group.clone()?,
                        })
                    },
                    cx,
                ));
            }
        }
        children
    }

    /// The stream form's own inputs (XADD's field / value / hex block is
    /// the shared one, drawn by the caller).
    fn render_stream_inputs(
        &self,
        form: &EditForm,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        let Some(inputs) = &self.inputs else {
            return div().into_any_element();
        };
        let labeled = |label: &'static str, input: &Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap(px(s(2.0)))
                .child(note_text(crate::localization::text(label)))
                .child(Input::new(input).w_full())
                .into_any_element()
        };
        let mut body = div().flex().flex_col().gap(px(s(6.0)));
        match form {
            EditForm::StreamAppend => {
                body = body.child(labeled(
                    "explorer.redis_key_view.stream_append_id_label",
                    &inputs.stream_id,
                ));
                if !self.stream.pairs.is_empty() {
                    let listed = self
                        .stream
                        .pairs
                        .iter()
                        .map(|(f, v)| format!("{} {}", quote_argument(f), quote_argument(v)))
                        .collect::<Vec<_>>()
                        .join("  ");
                    body = body.child(note_text(crate::localization::format(
                        "explorer.redis_key_view.stream_fields_so_far",
                        &[("fields", &listed)],
                    )));
                }
                body = body.child(note_text(crate::localization::text(
                    "explorer.redis_key_view.stream_add_pair_explanation",
                )));
            }
            EditForm::StreamTrim => {
                let strategy = self.stream.trim_strategy;
                let mut toggle = div().flex().flex_row().items_center().gap(px(s(2.0)));
                for option in [TrimStrategy::MaxLen, TrimStrategy::MinId] {
                    toggle = toggle.child(self.chip(
                        SharedString::from(format!("redis-stream-trim-{}", option.keyword())),
                        crate::localization::text(match option {
                            TrimStrategy::MaxLen => "explorer.redis_key_view.trim_maxlen_option",
                            TrimStrategy::MinId => "explorer.redis_key_view.trim_minid_option",
                        }),
                        strategy == option,
                        move |this, cx| {
                            this.stream.trim_strategy = option;
                            this.armed = false;
                            cx.notify();
                        },
                        cx,
                    ));
                }
                body = body.child(toggle).child(labeled(
                    match strategy {
                        TrimStrategy::MaxLen => "explorer.redis_key_view.trim_maxlen_label",
                        TrimStrategy::MinId => "explorer.redis_key_view.trim_minid_label",
                    },
                    &inputs.number,
                ));
                if let (TrimStrategy::MaxLen, Some(total), Ok(keep)) = (
                    strategy,
                    self.total,
                    self.input_text(|i| &i.number, cx).trim().parse::<i64>(),
                ) {
                    body = body.child(note_text(crate::localization::format(
                        "explorer.redis_key_view.trim_impact",
                        &[
                            ("total", &total.to_string()),
                            ("removed", &(total - keep).max(0).to_string()),
                        ],
                    )));
                }
                body = body.child(
                    Checkbox::new("redis-stream-trim-approx")
                        .label(crate::localization::text(
                            "explorer.redis_key_view.trim_approximate_checkbox",
                        ))
                        .checked(self.stream.trim_approximate)
                        .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                            this.stream.trim_approximate = *checked;
                            this.armed = false;
                            cx.notify();
                        })),
                );
            }
            EditForm::StreamGroupCreate => {
                body = body
                    .child(labeled(
                        "explorer.redis_key_view.group_name_label",
                        &inputs.group,
                    ))
                    .child(labeled(
                        "explorer.redis_key_view.group_start_label",
                        &inputs.stream_id,
                    ));
            }
            EditForm::StreamClaim { .. } => {
                body = body
                    .child(labeled(
                        "explorer.redis_key_view.claim_consumer_label",
                        &inputs.consumer,
                    ))
                    .child(labeled(
                        "explorer.redis_key_view.claim_idle_label",
                        &inputs.number,
                    ));
            }
            EditForm::StreamAutoClaim { .. } => {
                body = body
                    .child(labeled(
                        "explorer.redis_key_view.auto_claim_consumer_label",
                        &inputs.consumer,
                    ))
                    .child(labeled(
                        "explorer.redis_key_view.auto_claim_idle_label",
                        &inputs.number,
                    ))
                    .child(labeled(
                        "explorer.redis_key_view.auto_claim_start_label",
                        &inputs.stream_id,
                    ));
            }
            _ => {}
        }
        body.into_any_element()
    }

    /// The typed-confirmation input, when the form's action asks for one.
    fn render_typed_confirmation(
        &self,
        edit: &KeyEdit,
        cx: &gpui::App,
    ) -> Option<gpui::AnyElement> {
        let KeyEdit::Stream(action) = edit else {
            return None;
        };
        let expected = action.typed_confirmation()?;
        let inputs = self.inputs.as_ref()?;
        let s = Theme::scaled;
        let ok = self.typed_confirmation_ok(edit, cx);
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(s(2.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(if ok {
                            Theme::text_dim()
                        } else {
                            Theme::warning()
                        })
                        .child(crate::localization::format(
                            "explorer.redis_key_view.typed_confirmation",
                            &[("expected", &expected)],
                        )),
                )
                .child(Input::new(&inputs.confirm).w_full())
                .into_any_element(),
        )
    }
}

/// What the grid's rows are called for a type.
fn element_name(kind: &str) -> String {
    crate::localization::text(match kind {
        "hash" => "explorer.redis_key_view.element_fields",
        "list" => "explorer.redis_key_view.element_elements",
        "set" | "zset" => "explorer.redis_key_view.element_members",
        "stream" => "explorer.redis_key_view.element_entries",
        _ => "explorer.redis_key_view.element_rows",
    })
}

impl gpui::Focusable for RedisKeyViewPane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RedisKeyViewPane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        self.ensure_inputs(window, cx);
        self.sync_title_inputs();
        if self.windowed {
            // A popped-out view retargeted to another key re-titles its window.
            let title = self.title();
            if self.applied_window_title.as_ref() != Some(&title) {
                window.set_window_title(&title);
                self.applied_window_title = Some(title);
            }
        }
        let s = Theme::scaled;
        let focus_handle = self.focus_handle.clone();
        let mut body = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .p(px(s(14.0)));
        if self.key.is_none() {
            body = body.child(self.render_key_prompt(cx));
        } else {
            body = body.child(self.render_header(cx));
            body = body.child(self.render_grid_section(cx));
            body = body.child(self.render_detail(cx));
            body = body.child(self.render_actions(cx));
            if let Some(form) = self.form.clone() {
                body = body.child(self.render_form(&form, cx));
            }
        }
        if let Some(reason) = &self.write_block {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::warning())
                    .child(reason.clone()),
            );
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::error())
                    .child(error.clone()),
            );
        }
        let mut dialog = Dialog::new("redis-key-view", |this: &mut Self| &mut this.frame)
            .title(self.title())
            .easy_close(true)
            .key_context("RedisKeyView")
            .track_focus(&focus_handle)
            .windowed(self.windowed)
            .on_close(|this, window, cx| this.close(window, cx))
            // bare_body: the embedded grid must not sit in a scroll container.
            .bare_body()
            .child(body)
            .leading_button(
                Button::new("redis-key-view-refresh")
                    .label(crate::localization::text(
                        "explorer.redis_key_view.refresh_button",
                    ))
                    .disabled(self.loading || self.key.is_none())
                    .on_click(cx.listener(|this, _, _window, cx| this.refresh(cx))),
            )
            .leading_button(
                Button::new("redis-key-view-load-more")
                    .label(crate::localization::text(
                        "explorer.redis_key_view.load_more_button",
                    ))
                    .disabled(if self.on_pending_tab() {
                        self.stream.loading || self.stream.pending.next.is_none()
                    } else {
                        self.loading || self.next.is_none()
                    })
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if this.on_pending_tab() {
                            let next = this.stream.pending.next.clone();
                            if next.is_some() {
                                this.load_pending(next, cx);
                            }
                            return;
                        }
                        let next = this.next.clone();
                        if next.is_some() {
                            this.load_page(next, cx);
                        }
                    })),
            );
        if !self.windowed {
            dialog = dialog.detachable(|this, _window, cx| this.pop_out(cx));
        }
        dialog.build(self, window, cx)
    }
}

impl RedisKeyViewPane {
    fn render_key_prompt(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let Some(inputs) = &self.inputs else {
            return div().into_any_element();
        };
        div()
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "explorer.redis_key_view.open_prompt",
                        &[
                            ("database", &self.database_label()),
                            ("connection", &self.context.profile.name),
                        ],
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(Input::new(&inputs.key).w_full())
                    .child(
                        crate::theme::accent_pill_button(
                            "redis-key-view-open",
                            crate::localization::text("explorer.redis_key_view.open_button"),
                        )
                        .on_click(cx.listener(|this, _, _window, cx| this.open_typed_key(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_header(&self, _cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let mut strip = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(14.0)))
            .px(px(s(10.0)))
            .py(px(s(6.0)))
            .rounded(px(s(4.0)))
            .bg(Theme::sidebar_bg())
            .border_1()
            .border_color(Theme::border());
        match &self.header {
            Some(header) => {
                for (label, value) in header.strip() {
                    strip = strip.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap(px(s(4.0)))
                            .child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.78)))
                                    .text_color(Theme::text_dim())
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.88)))
                                    .text_color(Theme::text())
                                    .child(value),
                            ),
                    );
                }
            }
            None => {
                strip = strip.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(if self.loading {
                            "explorer.redis_key_view.header_loading"
                        } else {
                            "explorer.redis_key_view.header_empty"
                        })),
                );
            }
        }
        strip.into_any_element()
    }

    fn render_grid_section(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let kind = self.kind().to_string();
        let title = if kind.is_empty() {
            crate::localization::text("explorer.redis_key_view.value_title")
        } else {
            let (kind_name, coverage) = if kind == "stream" {
                (display_kind(&kind), self.stream_coverage_text())
            } else {
                (display_kind(&kind), self.coverage_text())
            };
            crate::localization::format(
                "explorer.redis_key_view.typed_value_title",
                &[("kind", &kind_name), ("coverage", &coverage)],
            )
        };
        let stream_tabs = (kind == "stream").then(|| self.render_stream_tabs(cx));
        let body = match &self.grid {
            _ if kind == "stream" && self.stream.tab != StreamTab::Entries => {
                div().size_full().child(self.render_stream_tab_body())
            }
            Some(grid) => div().size_full().child(grid.clone().into_any_element()),
            None if self.loading => div()
                .text_size(px(Theme::font_size_ratio(0.92)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "explorer.redis_key_view.value_loading",
                )),
            None if self.error.is_some() => div()
                .text_size(px(Theme::font_size_ratio(0.92)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "explorer.redis_key_view.value_unavailable",
                )),
            None if !kind.is_empty() && page_command(&kind, b"k", None, 1).is_none() => div()
                .text_size(px(Theme::font_size_ratio(0.92)))
                .text_color(Theme::text_dim())
                .child(crate::localization::format(
                    "explorer.redis_key_view.no_reader",
                    &[("kind", &kind)],
                )),
            None => div()
                .text_size(px(Theme::font_size_ratio(0.92)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "explorer.redis_key_view.value_empty",
                )),
        };
        div()
            .flex()
            .flex_col()
            .flex_basis(px(0.0))
            .flex_1()
            .min_h(px(s(120.0)))
            .gap(px(s(4.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.86)))
                    .text_color(Theme::text())
                    .child(title),
            )
            .children(stream_tabs)
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

    fn format_button(&self, format: ValueFormat, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let active = self.format == format;
        div()
            .id(SharedString::from(format!("redis-key-format-{format:?}")))
            .cursor_pointer()
            .px(px(s(8.0)))
            .py(px(s(3.0)))
            .rounded(px(s(4.0)))
            .text_size(px(Theme::font_size_ratio(0.88)))
            .when(active, |d| {
                d.bg(Theme::accent().opacity(0.2))
                    .text_color(Theme::accent())
            })
            .when(!active, |d| {
                d.text_color(Theme::text_dim())
                    .hover(|st| st.bg(Theme::row_hover()))
            })
            .child(format.label())
            .on_click(cx.listener(move |this: &mut Self, _event, _window, cx| {
                this.set_format(format, cx);
            }))
            .into_any_element()
    }

    /// Pick (or with `None` clear) the Protobuf message type for this key;
    /// remembered on disk like the key's format.
    fn set_message_choice(
        &mut self,
        choice: Option<crate::protobuf_schema::MessageChoice>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.message_choice = choice.clone();
        if let Some(id) = self.format_identity()
            && let Err(error) = crate::protobuf_schema::remember_message(&id, choice)
        {
            self.format_note = Some(error);
        }
        cx.notify();
    }

    /// The Protobuf message-type menu (custom schemas), shown while the
    /// value is read as Protobuf and a schema is registered.
    fn message_menu(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        use gpui_component::Sizable as _;
        use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};

        let this = cx.entity();
        let schemas = crate::protobuf_schema::schemas();
        let current = self.message_choice.clone();
        let none = crate::localization::text("results.structured_data.message_picker_none");
        let label = crate::localization::format(
            "results.structured_data.message_picker_label",
            &[(
                "message",
                current
                    .as_ref()
                    .map(|choice| choice.message.as_str())
                    .unwrap_or(none.as_str()),
            )],
        );
        Button::new("redis-key-message-menu")
            .ghost()
            .xsmall()
            .label(label)
            .dropdown_menu(move |menu, _window, _cx| {
                let none_this = this.clone();
                let mut menu = menu.item(
                    PopupMenuItem::new(none.clone())
                        .checked(current.is_none())
                        .on_click(move |_event, _window, app| {
                            none_this.update(app, |pane, cx| pane.set_message_choice(None, cx));
                        }),
                );
                for summary in &schemas {
                    let Ok(messages) = &summary.messages else {
                        continue;
                    };
                    menu = menu.separator();
                    for message in messages {
                        let choice = crate::protobuf_schema::MessageChoice {
                            schema_id: summary.entry.id.clone(),
                            message: message.clone(),
                        };
                        let checked = current.as_ref() == Some(&choice);
                        let this = this.clone();
                        menu = menu.item(
                            PopupMenuItem::new(crate::protobuf_schema::choice_label(
                                &choice, &schemas,
                            ))
                            .checked(checked)
                            .on_click(move |_event, _window, app| {
                                let choice = choice.clone();
                                this.update(app, |pane, cx| {
                                    pane.set_message_choice(Some(choice), cx)
                                });
                            }),
                        );
                    }
                }
                menu
            })
            .into_any_element()
    }

    fn render_detail(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        if self.kind() == "stream" && self.stream.tab != StreamTab::Entries {
            return div().into_any_element();
        }
        let Some(bytes) = self.detail_bytes() else {
            return div().into_any_element();
        };
        let shown = &bytes[..bytes.len().min(DETAIL_MAX_BYTES)];
        let rendered = match (&self.message_choice, self.format) {
            (Some(choice), ValueFormat::Protobuf) => {
                crate::protobuf_schema::render_with_choice(shown, choice)
            }
            _ => render_payload(shown, self.format),
        };
        let mut notes: Vec<String> = Vec::new();
        if bytes.len() > DETAIL_MAX_BYTES {
            notes.push(crate::localization::format(
                "explorer.redis_key_view.detail_truncated",
                &[
                    (
                        "shown",
                        &crate::background_tasks::human_bytes(DETAIL_MAX_BYTES as u64),
                    ),
                    (
                        "total",
                        &crate::background_tasks::human_bytes(bytes.len() as u64),
                    ),
                ],
            ));
        }
        if let Some(note) = rendered.note {
            notes.push(note);
        }
        if let Some(note) = &self.format_note {
            notes.push(note.clone());
        }
        let mut toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "explorer.redis_key_view.byte_count",
                        &[("count", &bytes.len().to_string())],
                    )),
            );
        if bytes.len() <= FORMAT_PICKER_MAX_BYTES {
            let mut picker = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(2.0)))
                .p(px(s(2.0)))
                .rounded(px(s(5.0)))
                .border_1()
                .border_color(Theme::border());
            for format in ValueFormat::ALL {
                picker = picker.child(self.format_button(format, cx));
            }
            toolbar = toolbar.child(picker);
            if self.format == ValueFormat::Protobuf && crate::protobuf_schema::any_schemas() {
                toolbar = toolbar.child(self.message_menu(cx));
            }
        }
        for note in notes {
            toolbar = toolbar.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::warning())
                    .child(note),
            );
        }
        let text: String = rendered
            .text
            .lines()
            .take(400)
            .collect::<Vec<_>>()
            .join("\n");
        div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .child(toolbar)
            .child(
                div()
                    .id("redis-key-view-detail")
                    .max_h(px(s(150.0)))
                    .overflow_y_scroll()
                    .p(px(s(8.0)))
                    .rounded(px(s(4.0)))
                    .bg(Theme::sidebar_bg())
                    .border_1()
                    .border_color(Theme::border())
                    .font_family(Theme::editor_font_family())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text())
                    .whitespace_normal()
                    .child(text),
            )
            .into_any_element()
    }

    /// One edit button: opens `form` (built from the view at click time,
    /// so "selected" means the selection then).
    fn action_button(
        &self,
        id: &'static str,
        label: String,
        enabled: bool,
        form: impl Fn(&Self) -> Option<EditForm> + 'static,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        Button::new(id)
            .label(label)
            .disabled(!enabled)
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(form) = form(this) {
                    this.open_form(form, window, cx);
                }
            }))
            .into_any_element()
    }

    /// The per-type edit buttons.
    fn render_actions(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let kind = self.kind().to_string();
        let has_selection = self.selected_row().is_some();
        let can_write = self.write_block.is_none() && !self.loading && !self.running;
        let selected_bytes = |col: usize| {
            move |this: &Self| {
                this.selected_row()
                    .and_then(|r| r.get(col))
                    .and_then(cell_bytes)
            }
        };
        let mut children: Vec<gpui::AnyElement> = Vec::new();
        match kind.as_str() {
            "string" => {
                let whole = self
                    .total
                    .is_none_or(|t| t as usize <= self.string_bytes.len());
                let small = self.string_bytes.len() <= EDIT_MAX_BYTES;
                children.push(self.action_button(
                    "redis-key-edit-string",
                    crate::localization::text("explorer.redis_key_view.edit_string_button"),
                    can_write && whole && small,
                    |_| Some(EditForm::StringValue),
                    cx,
                ));
                if !whole {
                    children.push(note_text(crate::localization::text(
                        "explorer.redis_key_view.edit_requires_whole_value",
                    )));
                } else if !small {
                    children.push(note_text(crate::localization::text(
                        "explorer.redis_key_view.edit_too_large",
                    )));
                }
            }
            "hash" => {
                children.push(self.action_button(
                    "redis-key-hash-add",
                    crate::localization::text("explorer.redis_key_view.hash_add_button"),
                    can_write,
                    |_| Some(EditForm::HashAdd),
                    cx,
                ));
                children.push(self.action_button(
                    "redis-key-hash-edit",
                    crate::localization::text("explorer.redis_key_view.hash_edit_button"),
                    can_write && has_selection,
                    move |this| selected_bytes(0)(this).map(|field| EditForm::HashEdit { field }),
                    cx,
                ));
                children.push(self.action_button(
                    "redis-key-hash-del",
                    crate::localization::text("explorer.redis_key_view.hash_delete_button"),
                    can_write && has_selection,
                    move |this| selected_bytes(0)(this).map(|field| EditForm::HashDelete { field }),
                    cx,
                ));
            }
            "list" => {
                children.push(self.action_button(
                    "redis-key-list-set",
                    crate::localization::text("explorer.redis_key_view.list_set_button"),
                    can_write && has_selection,
                    |this| match this.selected_row().and_then(|r| r.first()) {
                        Some(CellValue::I64(index)) => Some(EditForm::ListSet { index: *index }),
                        _ => None,
                    },
                    cx,
                ));
                children.push(self.action_button(
                    "redis-key-list-lpush",
                    crate::localization::text("explorer.redis_key_view.list_push_head_button"),
                    can_write,
                    |_| Some(EditForm::ListPush { head: true }),
                    cx,
                ));
                children.push(self.action_button(
                    "redis-key-list-rpush",
                    crate::localization::text("explorer.redis_key_view.list_push_tail_button"),
                    can_write,
                    |_| Some(EditForm::ListPush { head: false }),
                    cx,
                ));
                children.push(self.action_button(
                    "redis-key-list-rem",
                    crate::localization::text("explorer.redis_key_view.list_remove_button"),
                    can_write && has_selection,
                    move |this| selected_bytes(1)(this).map(|value| EditForm::ListRemove { value }),
                    cx,
                ));
            }
            "set" => {
                children.push(self.action_button(
                    "redis-key-set-add",
                    crate::localization::text("explorer.redis_key_view.set_add_button"),
                    can_write,
                    |_| Some(EditForm::SetAdd),
                    cx,
                ));
                children.push(self.action_button(
                    "redis-key-set-rem",
                    crate::localization::text("explorer.redis_key_view.set_remove_button"),
                    can_write && has_selection,
                    move |this| {
                        selected_bytes(0)(this).map(|member| EditForm::SetRemove { member })
                    },
                    cx,
                ));
            }
            "zset" => {
                children.push(self.action_button(
                    "redis-key-zset-add",
                    crate::localization::text("explorer.redis_key_view.zset_add_button"),
                    can_write,
                    |_| Some(EditForm::ZsetAdd),
                    cx,
                ));
                children.push(self.action_button(
                    "redis-key-zset-score",
                    crate::localization::text("explorer.redis_key_view.zset_score_button"),
                    can_write && has_selection,
                    move |this| {
                        selected_bytes(1)(this).map(|member| EditForm::ZsetScore { member })
                    },
                    cx,
                ));
                children.push(self.action_button(
                    "redis-key-zset-rem",
                    crate::localization::text("explorer.redis_key_view.zset_remove_button"),
                    can_write && has_selection,
                    move |this| {
                        selected_bytes(1)(this).map(|member| EditForm::ZsetRemove { member })
                    },
                    cx,
                ));
            }
            "stream" => {
                children.extend(self.stream_actions(can_write, cx));
            }
            k if is_json_kind(k) => {
                let small = self
                    .detail_bytes()
                    .is_some_and(|b| b.len() <= EDIT_MAX_BYTES);
                children.push(self.action_button(
                    "redis-key-json-edit",
                    crate::localization::text("explorer.redis_key_view.json_edit_button"),
                    can_write && small,
                    |_| Some(EditForm::JsonDocument),
                    cx,
                ));
            }
            "" => {}
            _ => {
                children.push(note_text(crate::localization::text(
                    "explorer.redis_key_view.no_type_edits",
                )));
            }
        }
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(6.0)))
            .children(children)
            .into_any_element()
    }

    fn render_form(&self, form: &EditForm, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let Some(inputs) = &self.inputs else {
            return div().into_any_element();
        };
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .p(px(s(10.0)))
            .rounded(px(s(4.0)))
            .border_1()
            .border_color(Theme::border())
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text())
                    .child(form.title()),
            );
        if form.is_stream() {
            body = body.child(self.render_stream_inputs(form, cx));
        }
        if form.wants_field() {
            body = body.child(Input::new(&inputs.field).w_full());
        }
        if form.wants_score() {
            body = body.child(Input::new(&inputs.score).w_full());
        }
        if form.wants_value() {
            body = body.child(Textarea::new(&inputs.value).w_full()).child(
                Checkbox::new("redis-key-view-hex")
                    .label(crate::localization::text(
                        "explorer.redis_key_view.hex_checkbox",
                    ))
                    .checked(self.hex_mode)
                    .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                        this.hex_mode = *checked;
                        this.armed = false;
                        cx.notify();
                    })),
            );
        }
        if matches!(form, EditForm::StreamAppend) {
            body = body.child(
                div().flex().flex_row().child(
                    Button::new("redis-stream-add-pair")
                        .label(crate::localization::text(
                            "explorer.redis_key_view.add_pair_button",
                        ))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.add_stream_pair(window, cx)),
                        ),
                ),
            );
        }
        if let Some(framing) = self.recompress
            && form.wants_value()
        {
            body = body.child(
                Checkbox::new("redis-key-view-recompress")
                    .label(crate::localization::format(
                        "explorer.redis_key_view.recompress_checkbox",
                        &[("format_name", framing.label())],
                    ))
                    .checked(self.recompress_on)
                    .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                        this.recompress_on = *checked;
                        this.armed = false;
                        cx.notify();
                    })),
            );
        }
        if matches!(form, EditForm::StringValue) {
            body = body.child(
                Checkbox::new("redis-key-view-keepttl")
                    .label(crate::localization::text(
                        "explorer.redis_key_view.keep_ttl_checkbox",
                    ))
                    .checked(self.keep_ttl)
                    .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                        this.keep_ttl = *checked;
                        this.armed = false;
                        cx.notify();
                    })),
            );
        }
        let preview = match (self.key.as_deref(), self.build_edit(cx)) {
            (Some(key), Ok(edit)) => Ok((edit.command(key), edit.note())),
            (_, Err(error)) => Err(error),
            (None, _) => Err(crate::localization::text("explorer.redis_key_view.no_key")),
        };
        match &preview {
            Ok((command, note)) => {
                body = body.child(
                    div()
                        .id("redis-key-view-command")
                        .max_h(px(s(90.0)))
                        .overflow_y_scroll()
                        .p(px(s(8.0)))
                        .rounded(px(s(4.0)))
                        .bg(Theme::sidebar_bg())
                        .font_family(Theme::editor_font_family())
                        .border_1()
                        .border_color(Theme::border())
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text())
                        .whitespace_normal()
                        .child(command.clone()),
                );
                if let Some(note) = note {
                    body = body.child(note_text(note.clone()));
                }
            }
            Err(error) => {
                body = body.child(note_text(error));
            }
        }
        if self.armed {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::warning())
                    .child(crate::localization::text(
                        "explorer.redis_key_view.run_confirmation_explanation",
                    )),
            );
        }
        let built = self.build_edit(cx).ok();
        let typed_ok = built
            .as_ref()
            .is_none_or(|edit| self.typed_confirmation_ok(edit, cx));
        if let Some(confirm) = built
            .as_ref()
            .and_then(|edit| self.render_typed_confirmation(edit, cx))
        {
            body = body.child(confirm);
        }
        let can_run = preview.is_ok() && typed_ok && !self.running && self.write_block.is_none();
        body = body.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .child({
                    let b = Button::new("redis-key-view-run")
                        .label(crate::localization::text(if self.armed {
                            "explorer.redis_key_view.confirm_run_button"
                        } else if self.running {
                            "explorer.redis_key_view.running_button"
                        } else {
                            "explorer.redis_key_view.run_button"
                        }))
                        .disabled(!can_run);
                    let b = if form.is_removal() {
                        b.danger()
                    } else {
                        b.primary()
                    };
                    b.on_click(cx.listener(|this, _, _window, cx| this.run_edit(cx)))
                })
                .child(
                    Button::new("redis-key-view-cancel-edit")
                        .label(crate::localization::text(
                            "explorer.redis_key_view.cancel_edit_button",
                        ))
                        .on_click(cx.listener(|this, _, _window, cx| this.close_form(cx))),
                ),
        );
        body.into_any_element()
    }
}

fn note_text(text: impl Into<SharedString>) -> gpui::AnyElement {
    div()
        .text_size(px(Theme::font_size_ratio(0.82)))
        .text_color(Theme::text_dim())
        .child(text.into())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::query::safety::redis::parse_script;

    fn column(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: String::new(),
            nullable: true,
        }
    }

    #[test]
    fn redis_key_view_header_reads_by_name_and_renders_a_strip() {
        let set = ToolQuerySet {
            columns: [
                "length",
                "memory_bytes",
                "encoding",
                "ttl_seconds",
                "type",
                "key",
            ]
            .iter()
            .map(|n| column(n))
            .collect(),
            rows: vec![vec![
                CellValue::I64(42),
                CellValue::Null,
                CellValue::String("listpack".into()),
                CellValue::I64(3720),
                CellValue::String("zset".into()),
                CellValue::String("k".into()),
            ]],
        };
        let header = KeyHeader::from_set(&set).unwrap();
        assert_eq!(header.kind, "zset");
        assert_eq!(header.length, Some(42));
        assert_eq!(header.memory_bytes, None);
        let strip = header.strip();
        assert_eq!(strip[0], ("Type".to_string(), "sorted set".to_string()));
        assert_eq!(strip[1].1, "1h 2m (3720 s)");
        assert_eq!(strip[3].1, "not measured");
        assert_eq!(strip[4].1, "42");
        assert!(KeyHeader::from_set(&ToolQuerySet::default()).is_none());
    }

    #[test]
    fn redis_key_view_picks_the_reader_by_type_and_continues_at_next() {
        let k = b"user:1".as_slice();
        assert_eq!(info_command(k), "SQLLY.KEYINFO user:1");
        assert_eq!(
            page_command("string", k, None, 10).unwrap(),
            format!("SQLLY.STRPAGE user:1 0 {STRING_WINDOW_BYTES}")
        );
        assert_eq!(
            page_command("string", k, Some("1048576"), 10).unwrap(),
            format!("SQLLY.STRPAGE user:1 1048576 {STRING_WINDOW_BYTES}")
        );
        assert_eq!(
            page_command("hash", k, None, 1000).unwrap(),
            "SQLLY.HPAGE user:1 0 1000"
        );
        assert_eq!(
            page_command("hash", k, Some("3456"), 1000).unwrap(),
            "SQLLY.HPAGE user:1 3456 1000"
        );
        assert_eq!(
            page_command("list", k, Some("1000"), 1000).unwrap(),
            "SQLLY.LPAGE user:1 1000 1000"
        );
        assert_eq!(
            page_command("set", k, None, 500).unwrap(),
            "SQLLY.SPAGE user:1 0 500"
        );
        assert_eq!(
            page_command("zset", k, None, 1000).unwrap(),
            "SQLLY.ZPAGE user:1 0 1000"
        );
        assert_eq!(
            page_command("stream", k, None, 1000).unwrap(),
            "SQLLY.XPAGE user:1 - 1000"
        );
        assert_eq!(
            page_command("stream", k, Some("(5-0"), 1000).unwrap(),
            "SQLLY.XPAGE user:1 (5-0 1000"
        );
        assert_eq!(
            page_command("ReJSON-RL", k, None, 1000).unwrap(),
            "JSON.GET user:1"
        );
        assert!(page_command("ReJSON-RL", k, Some("x"), 1000).is_none());
        assert!(page_command("MBbloom--", k, None, 1000).is_none());
        // Binary keys are escaped, never lossy.
        assert_eq!(
            page_command("hash", &[0xff, b' '], None, 1).unwrap(),
            "SQLLY.HPAGE \"\\xff \" 0 1"
        );
    }

    #[test]
    fn redis_key_view_parses_pages_and_keeps_empty_trailers() {
        let set = ToolQuerySet {
            columns: ["field", "value", "next", "total"]
                .iter()
                .map(|n| column(n))
                .collect(),
            rows: vec![
                vec![
                    CellValue::String("a".into()),
                    CellValue::String("00014".into()),
                    CellValue::String("77".into()),
                    CellValue::I64(5000),
                ],
                vec![
                    CellValue::String("b".into()),
                    CellValue::Bytes(vec![0xff]),
                    CellValue::String("77".into()),
                    CellValue::I64(5000),
                ],
            ],
        };
        let page = parse_page(&set);
        assert_eq!(page.columns.len(), 2);
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.rows[0][1], CellValue::String("00014".into()));
        assert_eq!(page.next.as_deref(), Some("77"));
        assert_eq!(page.total, Some(5000));
        let empty = ToolQuerySet {
            columns: set.columns.clone(),
            rows: vec![vec![
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                CellValue::I64(0),
            ]],
        };
        let page = parse_page(&empty);
        assert!(page.rows.is_empty());
        assert_eq!(page.next, None);
        assert_eq!(page.total, Some(0));
        let json = ToolQuerySet {
            columns: vec![column("result")],
            rows: vec![vec![CellValue::String("{\"a\":1}".into())]],
        };
        let page = parse_page(&json);
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.total, None);
        assert_eq!(cell_bytes(&page.rows[0][0]).unwrap(), b"{\"a\":1}");
        assert_eq!(cell_bytes(&CellValue::I64(3)), None);
    }

    #[test]
    fn redis_key_view_commands_per_type() {
        let k = b"k".as_slice();
        let v = b"00014".to_vec();
        assert_eq!(
            KeyEdit::SetString {
                value: v.clone(),
                keep_ttl: true
            }
            .command(k),
            "SET k 00014 KEEPTTL"
        );
        assert_eq!(
            KeyEdit::SetString {
                value: v.clone(),
                keep_ttl: false
            }
            .command(k),
            "SET k 00014"
        );
        assert_eq!(
            KeyEdit::HashSet {
                field: b"f".to_vec(),
                value: b"two words".to_vec()
            }
            .command(k),
            "HSET k f \"two words\""
        );
        assert_eq!(
            KeyEdit::HashDel {
                field: b"f".to_vec()
            }
            .command(k),
            "HDEL k f"
        );
        assert_eq!(
            KeyEdit::ListSet {
                index: 3,
                value: v.clone()
            }
            .command(k),
            "LSET k 3 00014"
        );
        assert_eq!(
            KeyEdit::ListPush {
                head: true,
                value: v.clone()
            }
            .command(k),
            "LPUSH k 00014"
        );
        assert_eq!(
            KeyEdit::ListPush {
                head: false,
                value: v.clone()
            }
            .command(k),
            "RPUSH k 00014"
        );
        let rem = KeyEdit::ListRem { value: v.clone() };
        assert_eq!(rem.command(k), "LREM k 1 00014");
        assert!(rem.note().unwrap().contains("by value"));
        assert_eq!(
            KeyEdit::SetAdd {
                member: b"m".to_vec()
            }
            .command(k),
            "SADD k m"
        );
        assert_eq!(
            KeyEdit::SetRem {
                member: b"m".to_vec()
            }
            .command(k),
            "SREM k m"
        );
        assert_eq!(
            KeyEdit::ZsetAdd {
                member: b"m".to_vec(),
                score: b"1.50".to_vec(),
                existing_only: false
            }
            .command(k),
            "ZADD k 1.50 m"
        );
        assert_eq!(
            KeyEdit::ZsetAdd {
                member: b"m".to_vec(),
                score: b"-inf".to_vec(),
                existing_only: true
            }
            .command(k),
            "ZADD k XX -inf m"
        );
        assert_eq!(
            KeyEdit::ZsetRem {
                member: b"m".to_vec()
            }
            .command(k),
            "ZREM k m"
        );
        assert_eq!(
            KeyEdit::JsonSet {
                document: b"{\"a\":1}".to_vec()
            }
            .command(k),
            "JSON.SET k $ \"{\\\"a\\\":1}\""
        );
        // Each command is one script line the engine classifies as a write.
        for edit in [
            KeyEdit::SetString {
                value: v.clone(),
                keep_ttl: true,
            },
            KeyEdit::HashDel {
                field: b"f".to_vec(),
            },
            KeyEdit::ZsetRem {
                member: b"m".to_vec(),
            },
        ] {
            let text = edit.command(k);
            let commands = parse_script(&text).unwrap();
            assert_eq!(commands.len(), 1, "{text}");
            assert!(
                crate::mutation_review::analyze_redis(&text).is_some(),
                "{text} must be reviewed as a write"
            );
        }
    }

    /// Deterministic pseudo-random bytes (xorshift), so the property holds
    /// over thousands of arbitrary arrays without a crate.
    fn arbitrary_bytes(seed: &mut u64, max_len: usize) -> Vec<u8> {
        let mut next = || {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed
        };
        let len = (next() % (max_len as u64 + 1)) as usize;
        (0..len).map(|_| (next() & 0xff) as u8).collect()
    }

    #[test]
    fn redis_key_view_keeps_bytes_exact() {
        let key = b"wsa:bytes".as_slice();
        let mut cases: Vec<Vec<u8>> = vec![
            b"00014".to_vec(),
            Vec::new(),
            vec![0x00],
            vec![0xff, 0xfe],
            b"12345678901234567890".to_vec(),
            b"1e309".to_vec(),
            b"-0".to_vec(),
            b"  leading and trailing  ".to_vec(),
            b"quote\"back\\slash".to_vec(),
            b"line\nbreak\ttab\r".to_vec(),
            "héllo wörld ✓".as_bytes().to_vec(),
            vec![0xc3, 0x28],
            vec![0xe2, 0x82],
            vec![b'\x07', b'\x08', 0x1b, 0x7f],
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..2000 {
            cases.push(arbitrary_bytes(&mut seed, 64));
        }
        for value in cases {
            let text = KeyEdit::SetString {
                value: value.clone(),
                keep_ttl: false,
            }
            .command(key);
            let commands = parse_script(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(commands.len(), 1, "{text}");
            assert_eq!(commands[0].args[0], b"SET");
            assert_eq!(commands[0].args[1], key);
            assert_eq!(commands[0].args[2], value, "SET argument: {text}");
            // Field and value of an HSET, and a hex round trip through the
            // editor, are byte-exact too.
            let text = KeyEdit::HashSet {
                field: value.clone(),
                value: value.clone(),
            }
            .command(key);
            let commands = parse_script(&text).unwrap();
            assert_eq!(commands[0].args[2], value);
            assert_eq!(commands[0].args[3], value);
            assert_eq!(hex_to_bytes(&bytes_to_hex(&value)).unwrap(), value);
            let (shown, hex) = editor_text_for(&value);
            assert_eq!(input_bytes(&shown, hex).unwrap(), value);
            // A grid cell made from the bytes gives the bytes back.
            let cell = match String::from_utf8(value.clone()) {
                Ok(s) => CellValue::String(s),
                Err(e) => CellValue::Bytes(e.into_bytes()),
            };
            assert_eq!(cell_bytes(&cell).unwrap(), value);
        }
        assert!(hex_to_bytes("abc").is_err());
        assert!(hex_to_bytes("zz").is_err());
        assert_eq!(hex_to_bytes("0xff 00\n0A").unwrap(), vec![0xff, 0x00, 0x0a]);
    }

    #[test]
    fn redis_key_view_writes_blocked_on_read_only_and_locked_production() {
        let mut profile = sqlly_core::ConnectionProfile::sql_login(
            "r",
            "h",
            "u",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        profile.database_type = sqlly_core::DatabaseType::Redis;
        assert!(write_block_for(&profile, ProductionDecision::NotProduction).is_none());
        assert!(
            write_block_for(&profile, ProductionDecision::ReadOnly)
                .unwrap()
                .contains("Production is read-only")
        );
        assert!(write_block_for(&profile, ProductionDecision::WritesUnlocked).is_none());
        profile.enforce_read_only = true;
        assert!(
            write_block_for(&profile, ProductionDecision::NotProduction)
                .unwrap()
                .contains("read-only")
        );
        profile.enforce_read_only = false;
        profile.block_all_but_select = true;
        assert!(write_block_for(&profile, ProductionDecision::NotProduction).is_some());

        // The review: an unlocked production connection and a connection
        // that confirms data changes both ask; a plain connection does not;
        // a read never does.
        let plain = crate::mutation_review::SafetyFeatures::default();
        assert!(!confirmation_required(
            &plain,
            ProductionDecision::NotProduction,
            "HSET k f v"
        ));
        assert!(confirmation_required(
            &plain,
            ProductionDecision::WritesUnlocked,
            "HSET k f v"
        ));
        let confirming = crate::mutation_review::SafetyFeatures {
            use_confirmation_dialog: true,
            ..Default::default()
        };
        assert!(confirmation_required(
            &confirming,
            ProductionDecision::NotProduction,
            "ZREM k m"
        ));
        assert!(!confirmation_required(
            &confirming,
            ProductionDecision::NotProduction,
            "SQLLY.KEYINFO k"
        ));
    }

    /// The grid is drawn inside the real pane (inline card and popped-out
    /// layout) and must get room for its rows.
    #[gpui::test]
    fn redis_key_view_grid_gets_room_to_paint_its_rows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        for windowed in [false, true] {
            let context =
                crate::tool_dialog::unreachable_tool_context(sqlly_core::DatabaseType::Redis);
            let check: crate::tool_dialog::ProductionCheck =
                std::rc::Rc::new(|_| ProductionDecision::NotProduction);
            let (view, cx) = cx.add_window_view(|_window, cx| {
                RedisKeyViewPane::new(
                    context,
                    Some(b"wsa:test".to_vec()),
                    ProductionDecision::NotProduction,
                    check,
                    cx,
                )
            });
            cx.simulate_resize(gpui::size(px(1200.0), px(900.0)));
            cx.run_until_parked();
            view.update(cx, |pane, cx| {
                pane.windowed = windowed;
                pane.error = None;
                pane.loading = false;
                pane.generation += 1;
                pane.header = Some(KeyHeader {
                    kind: "hash".into(),
                    ttl_seconds: None,
                    encoding: Some("listpack".into()),
                    memory_bytes: Some(100),
                    length: Some(5),
                });
                let set = crate::tool_dialog::sample_tool_set(5);
                pane.columns = set.columns.clone();
                pane.rows = set.rows.clone();
                pane.total = Some(5);
                pane.rebuild_grid(cx);
                cx.notify();
            });
            cx.run_until_parked();
            view.read_with(cx, |pane, cx| {
                let grid = pane.grid.as_ref().expect("value grid");
                crate::tool_dialog::assert_grid_shows_rows(
                    grid,
                    1,
                    format!("key view (windowed: {windowed})"),
                    cx,
                );
                assert_eq!(pane.coverage_text(), "showing 5 of 5 fields");
            });
        }
    }

    // ----- stream tabs (WS-G) ---------------------------------------------

    fn stream_set(names: &[&str], rows: Vec<Vec<CellValue>>) -> ToolQuerySet {
        ToolQuerySet {
            columns: names.iter().map(|n| column(n)).collect(),
            rows,
        }
    }

    /// A stream pane with two groups, one dead consumer and one pending
    /// entry installed directly (the connection is unreachable).
    fn stream_pane(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<RedisKeyViewPane>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let context = crate::tool_dialog::unreachable_tool_context(sqlly_core::DatabaseType::Redis);
        let check: crate::tool_dialog::ProductionCheck =
            std::rc::Rc::new(|_| ProductionDecision::NotProduction);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            RedisKeyViewPane::new(
                context,
                Some(b"wsg:x".to_vec()),
                ProductionDecision::NotProduction,
                check,
                cx,
            )
        });
        cx.simulate_resize(gpui::size(px(1200.0), px(900.0)));
        cx.run_until_parked();
        view.update(cx, |pane, cx| {
            pane.error = None;
            pane.loading = false;
            pane.generation += 1;
            pane.stream.generation += 1;
            pane.header = Some(KeyHeader {
                kind: "stream".into(),
                ttl_seconds: None,
                encoding: Some("stream".into()),
                memory_bytes: Some(100),
                length: Some(6),
            });
            pane.columns = vec![column("id"), column("fields")];
            pane.rows = (1..=6)
                .map(|i| {
                    vec![
                        CellValue::String(format!("{i}-0")),
                        CellValue::String(format!("{{\"n\":\"{i:05}\"}}")),
                    ]
                })
                .collect();
            pane.total = Some(6);
            pane.rebuild_grid(cx);
            let groups = stream_set(
                &[
                    "group",
                    "consumers",
                    "pending",
                    "last_delivered_id",
                    "entries_read",
                    "lag",
                    "stream_length",
                ],
                vec![
                    vec![
                        CellValue::String("wsg:workers".into()),
                        CellValue::I64(1),
                        CellValue::I64(1),
                        CellValue::String("2-0".into()),
                        CellValue::I64(2),
                        CellValue::I64(4),
                        CellValue::I64(6),
                    ],
                    vec![
                        CellValue::String("wsg:audit".into()),
                        CellValue::I64(0),
                        CellValue::I64(0),
                        CellValue::String("6-0".into()),
                        CellValue::I64(6),
                        CellValue::I64(0),
                        CellValue::I64(6),
                    ],
                ],
            );
            pane.stream.groups = crate::redis_streams::parse_groups(&groups);
            pane.stream.groups_set = Some(groups);
            pane.stream.group = Some(b"wsg:workers".to_vec());
            pane.stream.consumers_set = Some(stream_set(
                &["name", "pending", "idle", "inactive"],
                vec![vec![
                    CellValue::String("dead".into()),
                    CellValue::I64(1),
                    CellValue::I64(93_000),
                    CellValue::I64(93_000),
                ]],
            ));
            pane.stream.pending = Page {
                columns: ["id", "consumer", "idle_ms", "deliveries"]
                    .iter()
                    .map(|n| column(n))
                    .collect(),
                rows: vec![vec![
                    CellValue::String("1-0".into()),
                    CellValue::String("dead".into()),
                    CellValue::I64(93_000),
                    CellValue::I64(1),
                ]],
                next: None,
                total: Some(1),
            };
            pane.stream.pending_loaded = true;
            cx.notify();
        });
        cx.run_until_parked();
        (view, cx)
    }

    #[test]
    fn redis_key_view_opens_compressed_values_for_editing() {
        let gz = crate::value_recompress::Framing::Gzip
            .encode(b"{\"n\":1}")
            .unwrap();
        let opened = recompress_prefill(&gz, ValueFormat::Auto).unwrap();
        assert_eq!(opened.framing, crate::value_recompress::Framing::Gzip);
        assert_eq!(opened.text, "{\"n\":1}");
        assert_eq!(
            recompress_prefill(&gz, ValueFormat::Gzip).map(|v| v.framing),
            Some(crate::value_recompress::Framing::Gzip)
        );
        // An explicit non-compressed view edits the raw bytes.
        assert!(recompress_prefill(&gz, ValueFormat::Hex).is_none());
        assert!(recompress_prefill(b"plain", ValueFormat::Auto).is_none());
    }

    #[gpui::test]
    fn redis_key_view_saves_an_edited_compressed_string_recompressed(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = stream_pane(cx);
        let gz = crate::value_recompress::Framing::Gzip
            .encode(b"{\"n\":1}")
            .unwrap();
        view.update(cx, |pane, cx| {
            pane.header = Some(KeyHeader {
                kind: "string".into(),
                ttl_seconds: None,
                encoding: Some("raw".into()),
                memory_bytes: Some(64),
                length: Some(gz.len() as i64),
            });
            pane.string_bytes = gz.clone();
            pane.rows = vec![vec![CellValue::Bytes(gz.clone())]];
            pane.total = Some(gz.len() as i64);
            cx.notify();
        });
        view.update_in(cx, |pane, window, cx| {
            pane.open_form(EditForm::StringValue, window, cx);
            assert_eq!(
                pane.recompress,
                Some(crate::value_recompress::Framing::Gzip)
            );
            assert_eq!(pane.value_text(cx), "{\"n\":1}");
            let value = pane.inputs.as_ref().unwrap().value.clone();
            value.update(cx, |state, cx| state.set_value("{\"n\":2}", window, cx));
        });
        view.read_with(cx, |pane, cx| {
            let KeyEdit::SetString { value, .. } = pane.build_edit(cx).unwrap() else {
                panic!("not a SET");
            };
            assert_eq!(
                crate::value_recompress::Framing::Gzip
                    .decode(&value)
                    .unwrap(),
                b"{\"n\":2}"
            );
        });
        // Unticking the box writes the text as typed.
        view.update(cx, |pane, _| pane.recompress_on = false);
        view.read_with(cx, |pane, cx| {
            let KeyEdit::SetString { value, .. } = pane.build_edit(cx).unwrap() else {
                panic!("not a SET");
            };
            assert_eq!(value, b"{\"n\":2}");
        });
    }

    #[gpui::test]
    fn redis_streams_tabs_show_groups_consumers_and_pending(cx: &mut gpui::TestAppContext) {
        let (view, cx) = stream_pane(cx);
        view.read_with(cx, |pane, _| {
            assert_eq!(pane.stream.tab, StreamTab::Entries);
            assert_eq!(pane.stream_coverage_text(), "showing 6 of 6 entries");
        });
        for (tab, rows, coverage) in [
            (StreamTab::Groups, 2, "2 groups"),
            (StreamTab::Consumers, 1, "1 consumers in wsg:workers"),
            (
                StreamTab::Pending,
                1,
                "showing 1 of 1 pending in wsg:workers",
            ),
        ] {
            view.update(cx, |pane, cx| pane.set_stream_tab(tab, cx));
            cx.run_until_parked();
            view.read_with(cx, |pane, cx| {
                let grid = pane.stream.grid.as_ref().expect("tab grid");
                crate::tool_dialog::assert_grid_shows_rows(grid, rows, tab.label(), cx);
                assert_eq!(pane.stream_coverage_text(), coverage);
            });
        }
        // The pending row reads back with its idle milliseconds.
        view.update(cx, |pane, _| pane.stream.selected = Some(0));
        view.read_with(cx, |pane, _| {
            let row = pane.stream_pending_selected().expect("pending row");
            assert_eq!(row.consumer, b"dead");
            assert_eq!(row.idle_ms, Some(93_000));
        });
    }

    #[gpui::test]
    fn redis_streams_trim_requires_the_typed_count(cx: &mut gpui::TestAppContext) {
        let (view, cx) = stream_pane(cx);
        view.update_in(cx, |pane, window, cx| {
            pane.open_form(EditForm::StreamTrim, window, cx);
            let number = pane.inputs.as_ref().unwrap().number.clone();
            RedisKeyViewPane::set_input(&number, "2".into(), window, cx);
        });
        view.update(cx, |pane, cx| {
            let edit = pane.build_edit(cx).expect("trim edit");
            assert_eq!(edit.command(b"wsg:x"), "XTRIM wsg:x MAXLEN 2");
            assert!(!pane.typed_confirmation_ok(&edit, cx));
            // Run refuses without sending anything.
            pane.run_edit(cx);
            assert!(!pane.running);
            assert!(
                pane.error
                    .as_deref()
                    .unwrap()
                    .contains("Type the confirmation")
            );
        });
        view.update_in(cx, |pane, window, cx| {
            let confirm = pane.inputs.as_ref().unwrap().confirm.clone();
            RedisKeyViewPane::set_input(&confirm, "20".into(), window, cx);
        });
        view.read_with(cx, |pane, cx| {
            let edit = pane.build_edit(cx).unwrap();
            assert!(!pane.typed_confirmation_ok(&edit, cx), "20 is not 2");
        });
        view.update_in(cx, |pane, window, cx| {
            let confirm = pane.inputs.as_ref().unwrap().confirm.clone();
            RedisKeyViewPane::set_input(&confirm, "2".into(), window, cx);
        });
        view.read_with(cx, |pane, cx| {
            let edit = pane.build_edit(cx).unwrap();
            assert!(pane.typed_confirmation_ok(&edit, cx));
        });
        // MINID takes an id, approximate adds ~, and the confirmation follows.
        view.update_in(cx, |pane, window, cx| {
            pane.stream.trim_strategy = TrimStrategy::MinId;
            pane.stream.trim_approximate = true;
            let number = pane.inputs.as_ref().unwrap().number.clone();
            RedisKeyViewPane::set_input(&number, "3-0".into(), window, cx);
        });
        view.read_with(cx, |pane, cx| {
            let edit = pane.build_edit(cx).unwrap();
            assert_eq!(edit.command(b"wsg:x"), "XTRIM wsg:x MINID ~ 3-0");
            assert!(!pane.typed_confirmation_ok(&edit, cx));
        });
    }

    #[gpui::test]
    fn redis_streams_destroy_group_requires_the_typed_name(cx: &mut gpui::TestAppContext) {
        let (view, cx) = stream_pane(cx);
        view.update(cx, |pane, cx| pane.set_stream_tab(StreamTab::Groups, cx));
        cx.run_until_parked();
        view.update(cx, |pane, _| pane.stream.selected = Some(1));
        view.update_in(cx, |pane, window, cx| {
            let group = pane.stream_group_selected().expect("selected group");
            assert_eq!(group, b"wsg:audit");
            pane.open_form(EditForm::StreamGroupDestroy { group }, window, cx);
        });
        view.update(cx, |pane, cx| {
            let edit = pane.build_edit(cx).unwrap();
            assert_eq!(edit.command(b"wsg:x"), "XGROUP DESTROY wsg:x wsg:audit");
            assert!(!pane.typed_confirmation_ok(&edit, cx));
            pane.run_edit(cx);
            assert!(!pane.running);
        });
        view.update_in(cx, |pane, window, cx| {
            let confirm = pane.inputs.as_ref().unwrap().confirm.clone();
            RedisKeyViewPane::set_input(&confirm, "wsg:audit".into(), window, cx);
        });
        view.read_with(cx, |pane, cx| {
            let edit = pane.build_edit(cx).unwrap();
            assert!(pane.typed_confirmation_ok(&edit, cx));
        });
    }

    #[gpui::test]
    fn redis_streams_forms_build_exact_commands(cx: &mut gpui::TestAppContext) {
        let (view, cx) = stream_pane(cx);
        // XADD: two pairs, the second straight from the inputs, bytes exact.
        view.update_in(cx, |pane, window, cx| {
            pane.open_form(EditForm::StreamAppend, window, cx);
            let inputs = pane.inputs.as_ref().unwrap();
            let (field, value) = (inputs.field.clone(), inputs.value.clone());
            RedisKeyViewPane::set_input(&field, "n".into(), window, cx);
            value.update(cx, |state, cx| state.set_value("00014", window, cx));
            pane.add_stream_pair(window, cx);
            RedisKeyViewPane::set_input(&field, "raw".into(), window, cx);
            value.update(cx, |state, cx| state.set_value("ff 00", window, cx));
            pane.hex_mode = true;
        });
        view.read_with(cx, |pane, cx| {
            let edit = pane.build_edit(cx).unwrap();
            let text = edit.command(b"wsg:x");
            assert_eq!(text, "XADD wsg:x * n 00014 raw \"\\xff\\x00\"");
            let commands = parse_script(&text).unwrap();
            assert_eq!(commands[0].args[6], vec![0xff, 0x00]);
            assert!(pane.typed_confirmation_ok(&edit, cx));
        });
        // Claim: prefilled with the selected entry's idle time.
        view.update(cx, |pane, cx| pane.set_stream_tab(StreamTab::Pending, cx));
        cx.run_until_parked();
        view.update(cx, |pane, _| pane.stream.selected = Some(0));
        view.update_in(cx, |pane, window, cx| {
            pane.open_form(
                EditForm::StreamClaim {
                    group: b"wsg:workers".to_vec(),
                    id: b"1-0".to_vec(),
                },
                window,
                cx,
            );
            let consumer = pane.inputs.as_ref().unwrap().consumer.clone();
            RedisKeyViewPane::set_input(&consumer, "alive".into(), window, cx);
        });
        view.read_with(cx, |pane, cx| {
            assert_eq!(
                pane.build_edit(cx).unwrap().command(b"wsg:x"),
                "XCLAIM wsg:x wsg:workers alive 93000 1-0"
            );
        });
        // Auto-claim: a minute idle, from the start, 100 at a time.
        view.update_in(cx, |pane, window, cx| {
            pane.open_form(
                EditForm::StreamAutoClaim {
                    group: b"wsg:workers".to_vec(),
                },
                window,
                cx,
            );
            let consumer = pane.inputs.as_ref().unwrap().consumer.clone();
            RedisKeyViewPane::set_input(&consumer, "alive".into(), window, cx);
        });
        view.read_with(cx, |pane, cx| {
            assert_eq!(
                pane.build_edit(cx).unwrap().command(b"wsg:x"),
                "XAUTOCLAIM wsg:x wsg:workers alive 60000 0-0 COUNT 100"
            );
        });
        // Group create starts at $ unless told otherwise; a bad id is refused.
        view.update_in(cx, |pane, window, cx| {
            pane.open_form(EditForm::StreamGroupCreate, window, cx);
            let group = pane.inputs.as_ref().unwrap().group.clone();
            RedisKeyViewPane::set_input(&group, "wsg:new".into(), window, cx);
        });
        view.read_with(cx, |pane, cx| {
            assert_eq!(
                pane.build_edit(cx).unwrap().command(b"wsg:x"),
                "XGROUP CREATE wsg:x wsg:new $"
            );
        });
        view.update_in(cx, |pane, window, cx| {
            let id = pane.inputs.as_ref().unwrap().stream_id.clone();
            RedisKeyViewPane::set_input(&id, "latest".into(), window, cx);
        });
        view.read_with(cx, |pane, cx| {
            assert!(pane.build_edit(cx).is_err());
        });
        // Acknowledge needs nothing typed.
        view.update_in(cx, |pane, window, cx| {
            pane.open_form(
                EditForm::StreamAck {
                    group: b"wsg:workers".to_vec(),
                    id: b"1-0".to_vec(),
                },
                window,
                cx,
            );
        });
        view.read_with(cx, |pane, cx| {
            let edit = pane.build_edit(cx).unwrap();
            assert_eq!(edit.command(b"wsg:x"), "XACK wsg:x wsg:workers 1-0");
            assert!(pane.typed_confirmation_ok(&edit, cx));
        });
    }

    #[gpui::test]
    fn redis_streams_writes_blocked_on_a_read_only_connection(cx: &mut gpui::TestAppContext) {
        let (view, cx) = stream_pane(cx);
        view.update_in(cx, |pane, window, cx| {
            pane.context.profile.enforce_read_only = true;
            pane.open_form(
                EditForm::StreamDelete {
                    id: b"1-0".to_vec(),
                },
                window,
                cx,
            );
            pane.run_edit(cx);
            assert!(!pane.running);
            assert!(pane.error.as_deref().unwrap().contains("read-only"));
        });
    }

    /// Live, through the real pane: a stream with two groups and one dead
    /// consumer shows the dead consumer's pending entries with their idle
    /// milliseconds, Auto-claim moves them to another consumer, and Trim
    /// refuses until the count is typed, then trims. Database 15, keys
    /// prefixed `wsg:`. Opt-in through `SQLLY_REDIS_URL`. The review runs
    /// with a connection that confirms data changes, so each write takes the
    /// confirming second click.
    #[gpui::test]
    fn redis_streams_live_dead_consumer_autoclaim_and_typed_trim(cx: &mut gpui::TestAppContext) {
        let Some(url) = std::env::var("SQLLY_REDIS_URL").ok() else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        crate::ensure_test_tokio_runtime();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut profile =
            sqlly_engines::redis::profile_from_url("wsg-live", &url).expect("profile");
        profile.database = Some("15".to_string());
        profile.auto_wrap_rollback = false;
        let services = crate::Services::new();
        let key = format!("wsg:live:{}", std::process::id());
        let q = quote_argument(key.as_bytes());
        let script = |text: String| {
            let outcome = crate::tokio_handle().block_on(
                crate::tool_dialog::execute_collect_with_cap(&services, &profile, &text, 1000),
            );
            assert!(outcome.error.is_none(), "{text}: {:?}", outcome.error);
        };
        let mut seed = vec![format!("DEL {q}")];
        for i in 1..=6 {
            seed.push(format!("XADD {q} {i}-0 n {i:05}"));
        }
        seed.push(format!("XGROUP CREATE {q} wsg:workers 0"));
        seed.push(format!("XGROUP CREATE {q} wsg:audit $"));
        seed.push(format!(
            "XREADGROUP GROUP wsg:workers dead COUNT 2 STREAMS {q} >"
        ));
        script(seed.join("\n"));
        std::thread::sleep(std::time::Duration::from_millis(80));

        let context = ToolDialogContext::new(profile.clone(), services.clone());
        let check: crate::tool_dialog::ProductionCheck =
            std::rc::Rc::new(|_| ProductionDecision::NotProduction);
        let open_key = key.clone().into_bytes();
        let (view, cx) = cx.add_window_view(|_window, cx| {
            RedisKeyViewPane::new(
                context,
                Some(open_key),
                ProductionDecision::NotProduction,
                check,
                cx,
            )
        });
        view.update(cx, |pane, _| {
            pane.test_safety = Some(crate::mutation_review::SafetyFeatures {
                use_confirmation_dialog: true,
                ..Default::default()
            });
        });
        cx.simulate_resize(gpui::size(px(1200.0), px(900.0)));
        cx.run_until_parked();
        view.read_with(cx, |pane, _| {
            assert_eq!(pane.error, None);
            assert_eq!(pane.kind(), "stream");
            assert_eq!(pane.rows.len(), 6);
            assert_eq!(pane.stream.groups.len(), 2, "{:?}", pane.stream.groups);
        });
        view.update(cx, |pane, cx| {
            pane.select_group(b"wsg:workers".to_vec(), cx);
            pane.set_stream_tab(StreamTab::Pending, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |pane, _| {
            assert_eq!(pane.stream.pending.rows.len(), 2);
            assert_eq!(pane.stream.pending.total, Some(2));
            let workers = pane
                .stream
                .groups
                .iter()
                .find(|g| g.name == b"wsg:workers")
                .unwrap();
            assert_eq!(workers.pending, Some(2));
            assert_eq!(workers.lag, Some(4));
            for row in &pane.stream.pending.rows {
                let row =
                    crate::redis_streams::pending_row(&pane.stream.pending.columns, row).unwrap();
                assert_eq!(row.consumer, b"dead");
                assert!(row.idle_ms.unwrap() >= 50, "{row:?}");
            }
            assert!(pane.stream.grid.is_some());
        });

        // Auto-claim everything idle at least 50 ms for "alive".
        view.update_in(cx, |pane, window, cx| {
            pane.open_form(
                EditForm::StreamAutoClaim {
                    group: b"wsg:workers".to_vec(),
                },
                window,
                cx,
            );
            let inputs = pane.inputs.as_ref().unwrap();
            let (consumer, number) = (inputs.consumer.clone(), inputs.number.clone());
            RedisKeyViewPane::set_input(&consumer, "alive".into(), window, cx);
            RedisKeyViewPane::set_input(&number, "50".into(), window, cx);
        });
        view.update(cx, |pane, cx| {
            pane.run_edit(cx);
            assert!(pane.armed, "the review asks for a confirming click");
            pane.run_edit(cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |pane, _| {
            assert_eq!(pane.error, None);
            assert_eq!(pane.stream.tab, StreamTab::Pending);
            assert_eq!(
                pane.stream.group.as_deref(),
                Some(b"wsg:workers".as_slice())
            );
            assert_eq!(pane.stream.pending.rows.len(), 2);
            for row in &pane.stream.pending.rows {
                let row =
                    crate::redis_streams::pending_row(&pane.stream.pending.columns, row).unwrap();
                assert_eq!(row.consumer, b"alive", "XAUTOCLAIM moved the entry");
            }
        });

        // Trim to 3: refused until the count is typed, then trimmed.
        view.update(cx, |pane, cx| pane.set_stream_tab(StreamTab::Entries, cx));
        view.update_in(cx, |pane, window, cx| {
            pane.open_form(EditForm::StreamTrim, window, cx);
            let number = pane.inputs.as_ref().unwrap().number.clone();
            RedisKeyViewPane::set_input(&number, "3".into(), window, cx);
        });
        view.update(cx, |pane, cx| pane.run_edit(cx));
        cx.run_until_parked();
        view.read_with(cx, |pane, _| {
            assert!(
                pane.error
                    .as_deref()
                    .unwrap()
                    .contains("Type the confirmation")
            );
            assert_eq!(pane.rows.len(), 6, "nothing was trimmed");
        });
        view.update_in(cx, |pane, window, cx| {
            let confirm = pane.inputs.as_ref().unwrap().confirm.clone();
            RedisKeyViewPane::set_input(&confirm, "3".into(), window, cx);
        });
        view.update(cx, |pane, cx| {
            pane.run_edit(cx);
            assert!(pane.armed, "the review asks for a confirming click");
            pane.run_edit(cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |pane, _| {
            assert_eq!(pane.error, None);
            assert_eq!(pane.total, Some(3));
            assert_eq!(pane.rows.len(), 3);
            assert_eq!(pane.rows[0][0], CellValue::String("4-0".into()));
        });

        script(format!("DEL {q}"));
    }
}
