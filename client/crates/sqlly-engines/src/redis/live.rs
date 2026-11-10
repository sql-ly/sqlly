//! # Redis live lane
//!
//! MONITOR capture and Pub/Sub subscriptions for the Profiler and Pub/Sub
//! viewer tools. The editor lane ([`super::open_client`] and the safety
//! classification in `sqlly_intel::query::safety::redis`) deliberately
//! refuses `MONITOR` and `SUBSCRIBE` because they take a request/response
//! connection over forever. This lane is the deliberate exception: every
//! session opens a DEDICATED connection whose whole job is one stream, and
//! dropping the session closes the socket — that is the stop, and it is the
//! only way the stream can end. Only the live tools use this lane; the
//! editor's refusal is untouched.
//!
//! On a Redis Cluster ([`super::cluster`]) the lane opens one dedicated
//! connection per master and merges their streams, tagging every entry with
//! the node it came from: MONITOR runs on every master (each sees only the
//! commands for its own slots), keyspace notifications are subscribed on
//! every master (they are node-local), ordinary channels and patterns on
//! one master (a cluster broadcasts `PUBLISH` to every node, so more would
//! only duplicate), and sharded channels (`SSUBSCRIBE`, entered with a
//! leading `#`) on the master that owns each channel's slot. Sharded
//! channels work on a standalone Redis 7 server too.

use std::collections::VecDeque;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use sqlly_core::{ConnectionProfile, StudioError, StudioResult};

/// Most payload bytes one Pub/Sub message keeps. A publisher can send up to
/// 512 MB per message and the viewer's ring holds up to 100,000 of them, so
/// only this much is kept for display; the rest is counted, not stored.
pub const MAX_STORED_PAYLOAD_BYTES: usize = 64 * 1024;

/// Most bytes kept of a MONITOR argument tail (a `SET` of a large value
/// prints the whole value). Longer tails keep this prefix plus a
/// "truncated" marker naming the full size.
pub const MAX_STORED_MONITOR_TEXT_BYTES: usize = 64 * 1024;

/// Most bytes kept of a channel or pattern name.
pub const MAX_STORED_NAME_BYTES: usize = 1024;

/// The longest prefix of `text` that is at most `max` bytes and ends on a
/// character boundary.
pub fn utf8_prefix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `text` itself when it fits in `max` bytes; otherwise its prefix plus a
/// marker saying how large the original was, so nothing reads as complete
/// when it is not.
fn capped_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    format!(
        "{} … (truncated, {} bytes)",
        utf8_prefix(text, max),
        text.len()
    )
}

/// What a live-tool entry costs to keep, in bytes (heap text plus the entry
/// itself). Drives the ring's byte budget.
pub trait StoredSize {
    fn stored_bytes(&self) -> usize;
}

impl<T: StoredSize> StoredSize for std::sync::Arc<T> {
    fn stored_bytes(&self) -> usize {
        (**self).stored_bytes()
    }
}

/// One parsed `MONITOR` line: the server's own timestamp, the logical
/// database and client the command came from, the command name (uppercased
/// for display and filtering) and the verbatim argument tail as the server
/// printed it.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorEntry {
    /// Server wall clock, seconds since the Unix epoch (microsecond
    /// resolution) as printed at the head of the line.
    pub server_seconds: f64,
    /// The `[db …]` prefix; `None` when the line carried no numeric db.
    pub database: Option<i64>,
    /// Who ran it: `ip:port`, `lua` for scripts, or a `unix:` socket path.
    pub client: String,
    /// The first argument, uppercased. Empty when the tail could not be
    /// quoted-parsed (the raw tail is still kept in `text`).
    pub command: String,
    /// The argument tail verbatim (`"GET" "my key"`), escapes intact — what
    /// the table shows and the substring filter matches.
    pub text: String,
    /// The cluster node (`host:port`) the line came from; `None` on a
    /// standalone server.
    pub node: Option<String>,
}

impl StoredSize for MonitorEntry {
    fn stored_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.client.len()
            + self.command.len()
            + self.text.len()
            + self.node.as_ref().map_or(0, String::len)
    }
}

/// Parse one `MONITOR` stream line:
/// `1751524000.123456 [0 127.0.0.1:51234] "GET" "mykey"`. Returns `None`
/// when the line does not have the timestamp + bracketed-prefix shape (it
/// is never partially right: either the whole prefix parses or the line is
/// not a monitor line). A leading `+` (the RESP simple-string marker) is
/// tolerated.
pub fn parse_monitor_line(line: &str) -> Option<MonitorEntry> {
    let line = line.trim().strip_prefix('+').unwrap_or(line.trim());
    let (timestamp, rest) = line.split_once(' ')?;
    let server_seconds: f64 = timestamp.trim().parse().ok()?;
    let rest = rest.trim_start();
    if !rest.starts_with('[') {
        return None;
    }
    let close = rest.find("] ")?;
    let prefix = &rest[1..close];
    let text = capped_text(rest[close + 2..].trim(), MAX_STORED_MONITOR_TEXT_BYTES);
    let (database, client) = match prefix.split_once(' ') {
        Some((db, client)) => (db.trim().parse::<i64>().ok(), client.trim().to_string()),
        None => (None, prefix.trim().to_string()),
    };
    let command = parse_quoted_args(&text)
        .into_iter()
        .next()
        .map(|name| name.to_ascii_uppercase())
        .unwrap_or_default();
    Some(MonitorEntry {
        server_seconds,
        database,
        client,
        command,
        text,
        node: None,
    })
}

/// Split a MONITOR argument tail (`"GET" "ke\"y" "\x41"`) into its
/// arguments, undoing the C-style quoting Redis prints: `\"`, `\\`, `\n`,
/// `\r`, `\t`, `\b`, `\a` and `\xHH`. A bare (unquoted) word is taken
/// verbatim so a malformed tail still yields its leading arguments.
pub fn parse_quoted_args(tail: &str) -> Vec<String> {
    let bytes = tail.as_bytes();
    let mut args = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] != b'"' {
            let start = i;
            while i < bytes.len() && bytes[i] != b' ' {
                i += 1;
            }
            args.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
            continue;
        }
        i += 1;
        let mut arg = Vec::new();
        while i < bytes.len() && bytes[i] != b'"' {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                i += 1;
                match bytes[i] {
                    b'n' => arg.push(b'\n'),
                    b'r' => arg.push(b'\r'),
                    b't' => arg.push(b'\t'),
                    b'b' => arg.push(0x08),
                    b'a' => arg.push(0x07),
                    // Byte-wise: the two bytes after `\x` need not be ASCII
                    // (a malformed tail can put a multi-byte character
                    // there), so never slice the `&str` at them.
                    b'x' if i + 2 < bytes.len() => {
                        match (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                            (Some(high), Some(low)) => {
                                arg.push(high << 4 | low);
                                i += 2;
                            }
                            _ => arg.push(b'x'),
                        }
                    }
                    other => arg.push(other),
                }
                i += 1;
            } else {
                arg.push(bytes[i]);
                i += 1;
            }
        }
        // Skip the closing quote when present.
        if i < bytes.len() {
            i += 1;
        }
        args.push(String::from_utf8_lossy(&arg).into_owned());
    }
    args
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The Profiler's row filter: command name (exact, case-insensitive),
/// client address (contains) and free text over the argument tail
/// (contains). Empty fields do not constrain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfilerFilter {
    pub command: String,
    pub client: String,
    pub substring: String,
}

/// Whether an entry passes every non-empty field of the filter.
///
/// Matching is ASCII case-insensitive and allocation-free: the view runs it
/// over up to the whole ring (100,000 entries) on every refresh, and MONITOR
/// text is ASCII anyway — Redis prints every non-printable or non-ASCII
/// byte as a `\xHH` escape.
pub fn monitor_entry_matches(entry: &MonitorEntry, filter: &ProfilerFilter) -> bool {
    let command = filter.command.trim();
    if !command.is_empty() && !entry.command.eq_ignore_ascii_case(command) {
        return false;
    }
    contains_ignore_ascii_case(&entry.client, filter.client.trim())
        && contains_ignore_ascii_case(&entry.text, filter.substring.trim())
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// A bounded oldest-first buffer shared by both live tools. Pushing past
/// the cap drops the OLDEST entry and counts the drop, so `dropped` is the
/// honest "how much did a busy server cost us" figure the banner quotes.
///
/// A buffer built with [`BoundedBuffer::with_byte_budget`] also bounds the
/// total [`StoredSize`] it holds: past the budget it evicts oldest-first the
/// same way (always keeping the newest entry), so a flood of large items
/// cannot grow memory without limit even under a generous entry cap.
pub struct BoundedBuffer<T> {
    cap: usize,
    entries: VecDeque<T>,
    dropped: u64,
    byte_budget: usize,
    bytes: usize,
    weigh: fn(&T) -> usize,
}

impl<T> BoundedBuffer<T> {
    /// Count-bounded only.
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            entries: VecDeque::new(),
            dropped: 0,
            byte_budget: usize::MAX,
            bytes: 0,
            weigh: |_| 0,
        }
    }

    /// Bounded by count AND by the total [`StoredSize`] of what it holds.
    pub fn with_byte_budget(cap: usize, byte_budget: usize) -> Self
    where
        T: StoredSize,
    {
        Self {
            byte_budget: byte_budget.max(1),
            weigh: <T as StoredSize>::stored_bytes,
            ..Self::new(cap)
        }
    }

    pub fn push(&mut self, item: T) {
        self.bytes = self.bytes.saturating_add((self.weigh)(&item));
        self.entries.push_back(item);
        self.evict_over_limits();
    }

    /// Drop oldest entries while over the count cap or the byte budget;
    /// each eviction counts as a drop. The newest entry always stays.
    fn evict_over_limits(&mut self) {
        while self.entries.len() > self.cap
            || (self.bytes > self.byte_budget && self.entries.len() > 1)
        {
            let Some(evicted) = self.entries.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub((self.weigh)(&evicted));
            self.dropped += 1;
        }
    }

    /// Change the cap, evicting oldest-first when the buffer is already
    /// over the new limit (each eviction counts as a drop).
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
        self.evict_over_limits();
    }

    /// Empty the buffer. The drop counter survives: it describes the
    /// capture, not the current contents.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    /// Oldest to newest.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Total [`StoredSize`] currently held (0 for a count-only buffer).
    pub fn stored_bytes(&self) -> usize {
        self.bytes
    }

    /// The byte budget (`usize::MAX` for a count-only buffer).
    pub fn byte_budget(&self) -> usize {
        self.byte_budget
    }
}

/// One subscription request from the Pub/Sub viewer's comma-separated
/// input: a plain name subscribes with `SUBSCRIBE`, a name containing glob
/// metacharacters (`*`, `?`, `[…]`) with `PSUBSCRIBE`. A leading `=` forces
/// a literal channel (`=orders[eu]` subscribes to the channel named
/// `orders[eu]`), so a channel whose name contains glob characters is still
/// reachable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscribeTarget {
    Channel(String),
    Pattern(String),
    /// A sharded channel (`SSUBSCRIBE`, Redis 7+), entered as `#name`.
    Sharded(String),
}

/// Parse the viewer's comma-separated list. Blank entries are dropped; an
/// entry starting with `=` is a literal channel (the one `=` is stripped),
/// and every other entry is classified by glob metacharacters.
pub fn parse_subscribe_targets(input: &str) -> Vec<SubscribeTarget> {
    input
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            if let Some(literal) = entry.strip_prefix('=') {
                return (!literal.is_empty())
                    .then(|| SubscribeTarget::Channel(literal.to_string()));
            }
            if let Some(sharded) = entry.strip_prefix('#') {
                let sharded = sharded.trim();
                return (!sharded.is_empty())
                    .then(|| SubscribeTarget::Sharded(sharded.to_string()));
            }
            Some(
                if entry
                    .bytes()
                    .any(|b| matches!(b, b'*' | b'?' | b'[' | b']'))
                {
                    SubscribeTarget::Pattern(entry.to_string())
                } else {
                    SubscribeTarget::Channel(entry.to_string())
                },
            )
        })
        .collect()
}

/// The viewer's "Key events" preset: the comma-separated input text that
/// subscribes to every key-event notification on logical database `db`
/// (`__keyevent@<db>__:*`), plus one keyspace subscription
/// (`__keyspace@<db>__:<pattern>`) per plain entry already in `input` — a
/// key pattern such as `user:*` typed before pressing the preset watches
/// those keys by name too. Entries that are already keyspace or key-event
/// channels are kept as typed, so pressing the preset twice is idempotent.
/// The result goes back into the input and through
/// [`parse_subscribe_targets`], so the user sees exactly what is subscribed.
pub fn key_events_preset(db: i64, input: &str) -> String {
    let mut entries = vec![format!("__keyevent@{db}__:*")];
    for entry in input.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let entry = entry.strip_prefix('=').unwrap_or(entry);
        let expanded = if is_notification_channel(entry) {
            entry.to_string()
        } else {
            format!("__keyspace@{db}__:{entry}")
        };
        if !entries.contains(&expanded) {
            entries.push(expanded);
        }
    }
    entries.join(", ")
}

/// Where a keyspace-notification channel name starts.
const KEYSPACE_PREFIX: &str = "__keyspace@";
/// Where a key-event notification channel name starts.
const KEYEVENT_PREFIX: &str = "__keyevent@";

/// True for a `__keyspace@<db>__:…` or `__keyevent@<db>__:…` channel name.
fn is_notification_channel(name: &str) -> bool {
    notification_tail(name, KEYSPACE_PREFIX).is_some()
        || notification_tail(name, KEYEVENT_PREFIX).is_some()
}

/// The part after `__:` of a notification channel with the given prefix:
/// the key for a keyspace channel, the event for a key-event channel.
fn notification_tail<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = name.strip_prefix(prefix)?;
    let (db, tail) = rest.split_once("__:")?;
    (!db.is_empty() && db.bytes().all(|b| b.is_ascii_digit())).then_some(tail)
}

/// Split a keyspace notification into its `(event, key)` halves: a
/// key-event message (`__keyevent@<db>__:expired` carrying the key) and a
/// keyspace message (`__keyspace@<db>__:user:42` carrying the event) both
/// answer the same way, so the viewer shows one event column and one key
/// column whichever side was subscribed. `None` for an ordinary message.
/// Pure so the row content is testable without a socket.
pub fn keyspace_notification(channel: &str, payload: &PubSubPayload) -> Option<(String, String)> {
    let payload_text = match payload {
        PubSubPayload::Text(text) => text.clone(),
        PubSubPayload::Bytes(bytes) => display_name(bytes),
    };
    if let Some(event) = notification_tail(channel, KEYEVENT_PREFIX) {
        return Some((event.to_string(), payload_text));
    }
    notification_tail(channel, KEYSPACE_PREFIX).map(|key| (payload_text, key.to_string()))
}

/// A channel or pattern name for display: valid UTF-8 verbatim, anything
/// else with each byte that is not printable ASCII written as `\xHH` (the
/// way `redis-cli` prints binary names), so a binary channel never shows
/// as a bare `?`.
pub fn display_name(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        if byte.is_ascii_graphic() || byte == b' ' {
            out.push(byte as char);
        } else {
            out.push_str(&format!("\\x{byte:02x}"));
        }
    }
    out
}

/// The payload half of a Pub/Sub message: valid UTF-8 shows as text,
/// anything else is kept as bytes so the table can say so honestly. Holds at
/// most [`MAX_STORED_PAYLOAD_BYTES`]; see [`PubSubMessage::payload_len`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PubSubPayload {
    Text(String),
    Bytes(Vec<u8>),
}

impl PubSubPayload {
    /// Bytes kept.
    pub fn len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Bytes(bytes) => bytes.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One shaped Pub/Sub message for the viewer's table.
#[derive(Clone, Debug, PartialEq)]
pub struct PubSubMessage {
    pub channel: String,
    /// The pattern a `PSUBSCRIBE` delivery matched; `None` for plain
    /// channel messages.
    pub pattern: Option<String>,
    /// What was kept of the payload: at most [`MAX_STORED_PAYLOAD_BYTES`].
    pub payload: PubSubPayload,
    /// The payload's full size as published, in bytes. Larger than
    /// `payload.len()` exactly when the payload was truncated.
    pub payload_len: usize,
    /// Local wall clock (milliseconds since the Unix epoch) when the
    /// capture saw the message — messages carry no server timestamp.
    pub received_millis: i64,
    /// The event name (`expired`, `set`, `del`, …) when the message is a
    /// keyspace notification on either channel family; `None` otherwise.
    pub event: Option<String>,
    /// The key the notification is about; `None` for ordinary messages.
    pub key: Option<String>,
    /// The cluster node (`host:port`) that delivered the message; `None`
    /// on a standalone server.
    pub node: Option<String>,
}

impl PubSubMessage {
    /// True when only a prefix of the payload was kept.
    pub fn payload_truncated(&self) -> bool {
        self.payload_len > self.payload.len()
    }
}

impl StoredSize for PubSubMessage {
    fn stored_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.channel.len()
            + self.pattern.as_ref().map_or(0, String::len)
            + self.payload.len()
            + self.event.as_ref().map_or(0, String::len)
            + self.key.as_ref().map_or(0, String::len)
            + self.node.as_ref().map_or(0, String::len)
    }
}

/// Shape a message from its raw parts; pure so the viewer's row content is
/// testable without a socket. Channel and pattern arrive as raw bytes and
/// go through [`display_name`], so binary names stay distinguishable.
///
/// Only the first [`MAX_STORED_PAYLOAD_BYTES`] of the payload are kept (a
/// text payload is cut on a character boundary); `payload_len` records the
/// full size. A payload is text when the kept prefix is valid UTF-8 apart
/// from a character split by the cut.
pub fn shape_pubsub_message(
    channel: &[u8],
    pattern: Option<&[u8]>,
    payload: &[u8],
    received_millis: i64,
) -> PubSubMessage {
    let kept = &payload[..payload.len().min(MAX_STORED_PAYLOAD_BYTES)];
    let truncated = kept.len() < payload.len();
    let payload_value = match std::str::from_utf8(kept) {
        Ok(text) => PubSubPayload::Text(text.to_string()),
        // The cut split a multi-byte character: keep the text before it.
        Err(error) if truncated && error.error_len().is_none() => {
            PubSubPayload::Text(String::from_utf8_lossy(&kept[..error.valid_up_to()]).into_owned())
        }
        Err(_) => PubSubPayload::Bytes(kept.to_vec()),
    };
    let name = |bytes: &[u8]| {
        let shown = display_name(&bytes[..bytes.len().min(MAX_STORED_NAME_BYTES)]);
        if bytes.len() > MAX_STORED_NAME_BYTES {
            format!("{shown} … (truncated, {} bytes)", bytes.len())
        } else {
            shown
        }
    };
    let channel = name(channel);
    let (event, key) = keyspace_notification(&channel, &payload_value).unzip();
    PubSubMessage {
        channel,
        pattern: pattern.map(name),
        payload: payload_value,
        payload_len: payload.len(),
        received_millis,
        event,
        key,
        node: None,
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One frame off the MONITOR connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MonitorFrame {
    /// A command line, as the server printed it.
    Line(String),
    /// A command line from one cluster master (`node` is its `host:port`).
    NodeLine { node: String, line: String },
    /// The server sent an error reply on the monitoring connection.
    ServerError(String),
    /// A reply of an unexpected shape (not a status line); skipped.
    Other,
}

/// Classify one reply off the MONITOR connection. Pure so the mapping is
/// testable without a socket.
pub fn monitor_frame(value: redis::Value) -> MonitorFrame {
    match value {
        redis::Value::SimpleString(line) => MonitorFrame::Line(line),
        redis::Value::BulkString(bytes) => {
            MonitorFrame::Line(String::from_utf8_lossy(&bytes).into_owned())
        }
        redis::Value::ServerError(error) => {
            MonitorFrame::ServerError(match (error.code(), error.details()) {
                (code, Some(detail)) => format!("{code} {detail}"),
                (code, None) => code.to_string(),
            })
        }
        _ => MonitorFrame::Other,
    }
}

/// A live `MONITOR` capture on its own connection (one per master on a
/// cluster). Dropping it closes the socket(s), which ends the server-side
/// monitor.
pub struct MonitorCapture {
    stream: Pin<Box<dyn futures::Stream<Item = MonitorFrame> + Send>>,
    /// Ends each server-side monitor when the capture is dropped.
    _stop: Vec<MonitorStop>,
}

impl MonitorCapture {
    /// The next MONITOR frame, or `None` when the stream ended. The driver
    /// reports a closed connection, a dropped network and a reply it could
    /// not decode all the same way, so `None` means only "the stream
    /// ended", never specifically "the server closed it".
    pub async fn next_frame(&mut self) -> Option<MonitorFrame> {
        self.stream.next().await
    }
}

/// Frames buffered between the monitor connection's reader thread and the
/// capture. When the capture falls this far behind, the reader waits, which
/// back-pressures the socket instead of growing memory.
const MONITOR_CHANNEL_FRAMES: usize = 4096;

/// Drop guard for a monitor connection. The reader thread blocks on the
/// socket, so on drop the guard closes that connection server-side (`CLIENT
/// KILL ID`) from a short-lived second connection to the same endpoint; the
/// kill and the `PING` after it are commands the monitor itself sees, so
/// even a refused kill wakes the thread, which then finds the capture gone
/// and closes its socket.
struct MonitorStop {
    profile: ConnectionProfile,
    /// The endpoint the monitor connection dialed (after tunnelling; the
    /// node on a cluster, the current master on a Sentinel profile).
    host: String,
    port: u16,
    client_id: Option<i64>,
}

impl Drop for MonitorStop {
    fn drop(&mut self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let profile = self.profile.clone();
        let host = std::mem::take(&mut self.host);
        let port = self.port;
        let client_id = self.client_id;
        runtime.spawn(async move {
            let Ok(client) = super::build_client(&profile, &host, port) else {
                return;
            };
            let timeout = profile.timeout();
            let Ok(Ok(mut conn)) =
                tokio::time::timeout(timeout, client.get_multiplexed_async_connection()).await
            else {
                return;
            };
            if let Some(id) = client_id {
                let mut kill = redis::cmd("CLIENT");
                kill.arg("KILL").arg("ID").arg(id);
                let _ = tokio::time::timeout(timeout, kill.query_async::<redis::Value>(&mut conn))
                    .await;
            }
            let _ = tokio::time::timeout(
                timeout,
                redis::cmd("PING").query_async::<redis::Value>(&mut conn),
            )
            .await;
        });
    }
}

/// Open a dedicated connection from the profile, name it (see
/// [`super::setname`]) and put it in MONITOR mode. Tunnelling, TLS and ACL
/// credentials are exactly the query lane's, and it works the same on every
/// RESP-compatible server. A server that refuses MONITOR (a managed service
/// or an ACL rule can disable it) fails here with the server's own error
/// text. On a cluster it monitors every master (see the module docs).
pub async fn open_monitor(profile: &ConnectionProfile) -> StudioResult<MonitorCapture> {
    if profile.redis_sentinel.is_none() && super::cluster::is_known_cluster(profile) {
        return open_cluster_monitor(profile).await;
    }
    let first_host = super::cluster::single_host(profile);
    let profile = first_host.as_ref();
    // The current master on a Sentinel profile, the tunnel endpoint otherwise.
    let (host, port) = super::sentinel::data_endpoint(profile).await?;
    open_monitor_at(profile, &host, port).await
}

/// MONITOR on every master of a cluster, merged, each line tagged with its
/// node.
async fn open_cluster_monitor(profile: &ConnectionProfile) -> StudioResult<MonitorCapture> {
    let targets = super::cluster::master_dial_targets(profile).await?;
    if targets.is_empty() {
        return Err(StudioError::Connection(
            "the cluster named no masters to monitor".into(),
        ));
    }
    let node_profile = node_profile(profile);
    let mut streams = Vec::with_capacity(targets.len());
    let mut stops = Vec::with_capacity(targets.len());
    for (node, host, port) in targets {
        let MonitorCapture { stream, _stop } = open_monitor_at(&node_profile, &host, port).await?;
        stops.extend(_stop);
        streams.push(
            stream
                .map(move |frame| match frame {
                    MonitorFrame::Line(line) => MonitorFrame::NodeLine {
                        node: node.clone(),
                        line,
                    },
                    MonitorFrame::ServerError(error) => {
                        MonitorFrame::ServerError(format!("{node}: {error}"))
                    }
                    other => other,
                })
                .boxed(),
        );
    }
    Ok(MonitorCapture {
        stream: Box::pin(futures::stream::select_all(streams)),
        _stop: stops,
    })
}

/// A profile for one node's dedicated connection: database 0 (a cluster has
/// no other), and the server field cut to one host.
fn node_profile(profile: &ConnectionProfile) -> ConnectionProfile {
    let mut node = super::cluster::single_host(profile).into_owned();
    node.database = None;
    node
}

/// MONITOR on one dialed endpoint. The connection is read on its own thread
/// because the client library's async monitor cannot send `CLIENT SETNAME`
/// first, and the Profiler's own connection must be identifiable in
/// `CLIENT LIST`.
async fn open_monitor_at(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
) -> StudioResult<MonitorCapture> {
    let client = super::build_client(profile, host, port)?;
    let timeout = profile.timeout();
    let setname = super::setname::setname_cmd(profile);
    let (frames_tx, mut frames_rx) = tokio::sync::mpsc::channel(MONITOR_CHANNEL_FRAMES);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<StudioResult<Option<i64>>>();
    std::thread::Builder::new()
        .name("sqlly-redis-monitor".into())
        .spawn(move || monitor_reader(client, timeout, setname, ready_tx, frames_tx))
        .map_err(|e| StudioError::Connection(format!("cannot start the MONITOR reader: {e}")))?;
    // The thread applies `timeout` to connecting and to each setup reply;
    // the extra second covers the thread start itself.
    let client_id = tokio::time::timeout(timeout * 4 + Duration::from_secs(1), ready_rx)
        .await
        .map_err(|_| {
            StudioError::Connection(format!(
                "timed out opening the MONITOR connection after {timeout:?}"
            ))
        })?
        .map_err(|_| StudioError::Connection("the MONITOR connection closed".into()))??;
    let stream = futures::stream::poll_fn(move |cx| frames_rx.poll_recv(cx));
    Ok(MonitorCapture {
        stream: Box::pin(stream),
        _stop: vec![MonitorStop {
            profile: profile.clone(),
            host: host.to_string(),
            port,
            client_id,
        }],
    })
}

/// The monitor connection's thread: connect, name it, read its client id,
/// enter MONITOR, report readiness, then forward every frame until the
/// capture is dropped or the socket ends.
fn monitor_reader(
    client: redis::Client,
    timeout: Duration,
    setname: redis::Cmd,
    ready: tokio::sync::oneshot::Sender<StudioResult<Option<i64>>>,
    frames: tokio::sync::mpsc::Sender<MonitorFrame>,
) {
    let refused = |text: String| {
        StudioError::CapabilityNotSupported(format!(
            "the server refused the MONITOR connection: {text}"
        ))
    };
    let mut conn = match client.get_connection_with_timeout(timeout) {
        Ok(conn) => conn,
        Err(e) => {
            let _ = ready.send(Err(match super::redis_error(e) {
                StudioError::QueryExecution(text) => refused(text),
                other => other,
            }));
            return;
        }
    };
    let _ = conn.set_read_timeout(Some(timeout));
    let _ = conn.set_write_timeout(Some(timeout));
    if let Err(error) = setname.query::<redis::Value>(&mut conn) {
        tracing::debug!(%error, "CLIENT SETNAME refused on the MONITOR connection");
    }
    let client_id = redis::cmd("CLIENT").arg("ID").query::<i64>(&mut conn).ok();
    let entered = conn
        .send_packed_command(&redis::cmd("MONITOR").get_packed_command())
        .and_then(|()| conn.recv_response());
    match entered {
        Ok(redis::Value::ServerError(error)) => {
            let _ = ready.send(Err(refused(match error.details() {
                Some(detail) => format!("{} {detail}", error.code()),
                None => error.code().to_string(),
            })));
            return;
        }
        Ok(_) => {}
        Err(e) => {
            let _ = ready.send(Err(match super::redis_error(e) {
                StudioError::QueryExecution(text) => refused(text),
                other => other,
            }));
            return;
        }
    }
    // Monitoring may sit idle for minutes: no read deadline from here on.
    let _ = conn.set_read_timeout(None);
    if ready.send(Ok(client_id)).is_err() {
        return;
    }
    while let Ok(value) = conn.recv_response() {
        if frames.blocking_send(monitor_frame(value)).is_err() {
            break;
        }
    }
}

/// A live Pub/Sub subscription on its own connection. Dropping it closes
/// the socket, which unsubscribes every channel and pattern.
pub struct PubSubListener {
    stream: Pin<Box<dyn futures::Stream<Item = PubSubMessage> + Send>>,
}

impl PubSubListener {
    /// The next published message, or `None` when the subscription
    /// connection ended.
    pub async fn next_message(&mut self) -> Option<PubSubMessage> {
        self.stream.next().await
    }
}

/// One received message as a listener row. Raw bytes, not
/// `get_channel_name` (which turns a binary channel into "?") or
/// `Option<String>` (which drops a binary pattern).
fn message_row(msg: &redis::Msg) -> PubSubMessage {
    let channel = msg.get_channel::<Vec<u8>>().unwrap_or_default();
    let pattern = msg.get_pattern::<Option<Vec<u8>>>().ok().flatten();
    shape_pubsub_message(
        &channel,
        pattern.as_deref(),
        msg.get_payload_bytes(),
        now_millis(),
    )
}

/// Open a dedicated connection from the profile and subscribe it: plain
/// names with `SUBSCRIBE`, glob names with `PSUBSCRIBE`, sharded names with
/// `SSUBSCRIBE`. Refuses an empty target list before touching the network.
/// Each connection speaks RESP3 when the server accepts it, which lets it
/// name itself (see [`super::setname`]) before subscribing; a server without
/// RESP3 gets the classic RESP2 subscriber connection, which cannot send
/// `CLIENT SETNAME` and so stays unnamed.
pub async fn open_pubsub(
    profile: &ConnectionProfile,
    targets: &[SubscribeTarget],
) -> StudioResult<PubSubListener> {
    if targets.is_empty() {
        return Err(StudioError::InvalidInput(
            "no channels or patterns to subscribe to".into(),
        ));
    }
    let (sharded, classic): (Vec<&SubscribeTarget>, Vec<&SubscribeTarget>) = targets
        .iter()
        .partition(|t| matches!(t, SubscribeTarget::Sharded(_)));
    if profile.redis_sentinel.is_none() && super::cluster::is_known_cluster(profile) {
        return open_cluster_pubsub(profile, &classic, &sharded).await;
    }
    let first_host = super::cluster::single_host(profile);
    let profile = first_host.as_ref();
    // The current master on a Sentinel profile, the tunnel endpoint otherwise.
    let (host, port) = super::sentinel::data_endpoint(profile).await?;
    if sharded.is_empty() {
        return open_pubsub_at(profile, &host, port, targets, None).await;
    }
    let channels: Vec<String> = sharded.iter().map(|t| target_name(t).to_string()).collect();
    let mut streams = vec![open_sharded_at(profile, &host, port, &channels, None).await?];
    if !classic.is_empty() {
        let classic: Vec<SubscribeTarget> = classic.into_iter().cloned().collect();
        streams.push(open_pubsub_at(profile, &host, port, &classic, None).await?);
    }
    Ok(merge_listeners(streams))
}

fn target_name(target: &SubscribeTarget) -> &str {
    match target {
        SubscribeTarget::Channel(name)
        | SubscribeTarget::Pattern(name)
        | SubscribeTarget::Sharded(name) => name,
    }
}

fn merge_listeners(listeners: Vec<PubSubListener>) -> PubSubListener {
    if listeners.len() == 1 {
        return listeners.into_iter().next().expect("one listener");
    }
    PubSubListener {
        stream: Box::pin(futures::stream::select_all(
            listeners.into_iter().map(|l| l.stream),
        )),
    }
}

/// Pub/Sub on a cluster: keyspace notifications on every master (they are
/// node-local), other channels and patterns on the first master (`PUBLISH`
/// reaches every node), sharded channels on each channel's slot owner.
async fn open_cluster_pubsub(
    profile: &ConnectionProfile,
    classic: &[&SubscribeTarget],
    sharded: &[&SubscribeTarget],
) -> StudioResult<PubSubListener> {
    let masters = super::cluster::master_dial_targets(profile).await?;
    let Some(first) = masters.first().cloned() else {
        return Err(StudioError::Connection(
            "the cluster named no masters to subscribe on".into(),
        ));
    };
    let node_profile = node_profile(profile);
    let mut listeners = Vec::new();
    let (notifications, ordinary): (Vec<SubscribeTarget>, Vec<SubscribeTarget>) = classic
        .iter()
        .map(|t| (*t).clone())
        .partition(|t| is_notification_channel(target_name(t)));
    if !notifications.is_empty() {
        for (node, host, port) in &masters {
            listeners.push(
                open_pubsub_at(
                    &node_profile,
                    host,
                    *port,
                    &notifications,
                    Some(node.clone()),
                )
                .await?,
            );
        }
    }
    if !ordinary.is_empty() {
        let (node, host, port) = &first;
        listeners
            .push(open_pubsub_at(&node_profile, host, *port, &ordinary, Some(node.clone())).await?);
    }
    let mut by_owner: Vec<((String, String, u16), Vec<String>)> = Vec::new();
    for target in sharded {
        let name = target_name(target);
        let owner = super::cluster::slot_owner_dial_target(profile, name.as_bytes())
            .await?
            .unwrap_or_else(|| first.clone());
        match by_owner.iter_mut().find(|(o, _)| *o == owner) {
            Some((_, names)) => names.push(name.to_string()),
            None => by_owner.push((owner, vec![name.to_string()])),
        }
    }
    for ((node, host, port), names) in by_owner {
        listeners.push(open_sharded_at(&node_profile, &host, port, &names, Some(node)).await?);
    }
    Ok(merge_listeners(listeners))
}

/// `SSUBSCRIBE` on one endpoint: a RESP3 connection (sharded messages
/// arrive as `smessage` pushes, which only RESP3 delivers out of band),
/// named like every other connection, each message tagged with `node`.
async fn open_sharded_at(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
    channels: &[String],
    node: Option<String>,
) -> StudioResult<PubSubListener> {
    let timeout = profile.timeout();
    let client = super::build_client_with(profile, host, port, redis::ProtocolVersion::RESP3)?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<redis::PushInfo>();
    let config = redis::AsyncConnectionConfig::new()
        .set_connection_timeout(Some(timeout))
        .set_response_timeout(None)
        .set_push_sender(tx);
    let mut conn = tokio::time::timeout(
        timeout,
        client.get_multiplexed_async_connection_with_config(&config),
    )
    .await
    .map_err(|_| {
        StudioError::Connection(format!(
            "timed out opening the sharded Pub/Sub connection after {timeout:?}"
        ))
    })?
    .map_err(|e| match super::redis_error(e) {
        StudioError::QueryExecution(text) => StudioError::Connection(text),
        other => other,
    })?;
    let setname = super::setname::setname_cmd(profile);
    if let Ok(Err(error)) =
        tokio::time::timeout(timeout, setname.query_async::<redis::Value>(&mut conn)).await
    {
        tracing::debug!(%error, "CLIENT SETNAME refused on the sharded Pub/Sub connection");
    }
    for channel in channels {
        let mut cmd = redis::cmd("SSUBSCRIBE");
        cmd.arg(channel.as_str());
        tokio::time::timeout(timeout, cmd.exec_async(&mut conn))
            .await
            .map_err(|_| {
                StudioError::Connection(format!("timed out subscribing after {timeout:?}"))
            })?
            .map_err(|e| match super::redis_error(e) {
                StudioError::QueryExecution(text) => StudioError::CapabilityNotSupported(format!(
                    "the server refused SSUBSCRIBE {channel} (sharded Pub/Sub needs Redis 7 or later): {text}"
                )),
                other => other,
            })?;
    }
    let stream = futures::stream::unfold((rx, conn), |(mut rx, conn)| async move {
        loop {
            let push = rx.recv().await?;
            if push.kind == redis::PushKind::Disconnection {
                return None;
            }
            if push.kind != redis::PushKind::SMessage && push.kind != redis::PushKind::Message {
                continue;
            }
            let Some(msg) = redis::Msg::from_push_info(push) else {
                continue;
            };
            let channel = msg.get_channel::<Vec<u8>>().unwrap_or_default();
            let message =
                shape_pubsub_message(&channel, None, msg.get_payload_bytes(), now_millis());
            return Some((message, (rx, conn)));
        }
    })
    .map(move |mut message| {
        message.node = node.clone();
        message
    });
    Ok(PubSubListener {
        stream: Box::pin(stream),
    })
}

/// `SUBSCRIBE` / `PSUBSCRIBE` on one endpoint, each message tagged with
/// `node` (`None` on a standalone server): the RESP3 subscriber first, the
/// RESP2 one when the server refuses RESP3.
async fn open_pubsub_at(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
    targets: &[SubscribeTarget],
    node: Option<String>,
) -> StudioResult<PubSubListener> {
    let listener = match open_pubsub_resp3(profile, host, port, targets).await {
        Err(None) => open_pubsub_resp2(profile, host, port, targets).await?,
        Err(Some(error)) => return Err(error),
        Ok(listener) => listener,
    };
    let Some(node) = node else {
        return Ok(listener);
    };
    Ok(PubSubListener {
        stream: Box::pin(listener.stream.map(move |mut message| {
            message.node = Some(node.clone());
            message
        })),
    })
}

/// The RESP3 subscriber: one ordinary connection whose push messages carry
/// the published payloads. `Err(None)` means the server refused RESP3.
async fn open_pubsub_resp3(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
    targets: &[SubscribeTarget],
) -> Result<PubSubListener, Option<StudioError>> {
    let client = super::build_client_with(profile, host, port, redis::ProtocolVersion::RESP3)
        .map_err(Some)?;
    let timeout = profile.timeout();
    let (push_tx, mut push_rx) = tokio::sync::mpsc::unbounded_channel::<redis::PushInfo>();
    let config = redis::AsyncConnectionConfig::new()
        .set_connection_timeout(Some(timeout))
        .set_response_timeout(None)
        .set_push_sender(push_tx);
    let mut conn = tokio::time::timeout(
        timeout,
        client.get_multiplexed_async_connection_with_config(&config),
    )
    .await
    .map_err(|_| {
        Some(StudioError::Connection(format!(
            "timed out opening the Pub/Sub connection after {timeout:?}"
        )))
    })?
    .map_err(|e| {
        if super::hello::hello_refused(&e) {
            return None;
        }
        Some(match super::redis_error(e) {
            StudioError::QueryExecution(text) => StudioError::Connection(text),
            other => other,
        })
    })?;
    let setname = super::setname::setname_cmd(profile);
    if let Ok(Err(error)) =
        tokio::time::timeout(timeout, setname.query_async::<redis::Value>(&mut conn)).await
    {
        tracing::debug!(%error, "CLIENT SETNAME refused on the Pub/Sub connection");
    }
    for target in targets {
        let subscribed = match target {
            SubscribeTarget::Channel(name) => {
                tokio::time::timeout(timeout, conn.subscribe(name.as_str())).await
            }
            SubscribeTarget::Pattern(pattern) => {
                tokio::time::timeout(timeout, conn.psubscribe(pattern.as_str())).await
            }
            // Sharded channels have their own connection (`open_sharded_at`).
            SubscribeTarget::Sharded(_) => continue,
        };
        subscribed
            .map_err(|_| {
                Some(StudioError::Connection(format!(
                    "timed out subscribing after {timeout:?}"
                )))
            })?
            .map_err(|e| Some(super::redis_error(e)))?;
    }
    // The stream owns the connection: dropping the listener drops it, which
    // closes the socket and so unsubscribes everything.
    let stream = futures::stream::poll_fn(move |cx| {
        let _keep_open = &conn;
        loop {
            match push_rx.poll_recv(cx) {
                std::task::Poll::Ready(Some(info)) => {
                    if info.kind == redis::PushKind::Disconnection {
                        return std::task::Poll::Ready(None);
                    }
                    if let Some(msg) = redis::Msg::from_push_info(info) {
                        return std::task::Poll::Ready(Some(message_row(&msg)));
                    }
                    // Subscribe confirmations and other pushes: keep reading.
                }
                std::task::Poll::Ready(None) => return std::task::Poll::Ready(None),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    });
    Ok(PubSubListener {
        stream: Box::pin(stream),
    })
}

/// The RESP2 subscriber connection, for servers without RESP3.
async fn open_pubsub_resp2(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
    targets: &[SubscribeTarget],
) -> StudioResult<PubSubListener> {
    let client = super::build_client(profile, host, port)?;
    let timeout = profile.timeout();
    let mut pubsub = tokio::time::timeout(timeout, client.get_async_pubsub())
        .await
        .map_err(|_| {
            StudioError::Connection(format!(
                "timed out opening the Pub/Sub connection after {timeout:?}"
            ))
        })?
        .map_err(|e| match super::redis_error(e) {
            StudioError::QueryExecution(text) => StudioError::Connection(text),
            other => other,
        })?;
    for target in targets {
        let subscribed = match target {
            SubscribeTarget::Channel(name) => {
                tokio::time::timeout(timeout, pubsub.subscribe(name.as_str())).await
            }
            SubscribeTarget::Pattern(pattern) => {
                tokio::time::timeout(timeout, pubsub.psubscribe(pattern.as_str())).await
            }
            SubscribeTarget::Sharded(_) => continue,
        };
        subscribed
            .map_err(|_| {
                StudioError::Connection(format!("timed out subscribing after {timeout:?}"))
            })?
            .map_err(super::redis_error)?;
    }
    let stream = pubsub.into_on_message().map(|msg| message_row(&msg));
    Ok(PubSubListener {
        stream: Box::pin(stream),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_line_parses_timestamp_db_client_and_command() {
        let entry = parse_monitor_line("1751524000.123456 [0 127.0.0.1:51234] \"GET\" \"mykey\"")
            .expect("parses");
        assert!((entry.server_seconds - 1751524000.123456).abs() < 1e-6);
        assert_eq!(entry.database, Some(0));
        assert_eq!(entry.client, "127.0.0.1:51234");
        assert_eq!(entry.command, "GET");
        assert_eq!(entry.text, "\"GET\" \"mykey\"");
    }

    #[test]
    fn monitor_line_uppercases_lowercase_commands_and_keeps_args() {
        let entry = parse_monitor_line(
            "1751524001.000001 [15 10.0.0.2:6379] \"set\" \"greeting\" \"hello world\"",
        )
        .expect("parses");
        assert_eq!(entry.database, Some(15));
        assert_eq!(entry.command, "SET");
        assert_eq!(entry.text, "\"set\" \"greeting\" \"hello world\"");
    }

    #[test]
    fn monitor_line_handles_lua_and_unix_clients() {
        let lua = parse_monitor_line("1751524002.5 [0 lua] \"EVAL\" \"return 1\" \"0\"")
            .expect("lua parses");
        assert_eq!(lua.client, "lua");
        assert_eq!(lua.database, Some(0));
        let unix =
            parse_monitor_line("1751524003.5 [2 unix:/run/redis.sock] \"PING\"").expect("unix");
        assert_eq!(unix.client, "unix:/run/redis.sock");
        assert_eq!(unix.database, Some(2));
        assert_eq!(unix.command, "PING");
    }

    #[test]
    fn monitor_line_tolerates_resp_plus_marker() {
        let entry = parse_monitor_line("+1751524004.0 [0 127.0.0.1:1] \"PING\"").expect("parses");
        assert_eq!(entry.command, "PING");
    }

    #[test]
    fn monitor_line_rejects_non_monitor_shapes() {
        assert!(parse_monitor_line("").is_none());
        assert!(parse_monitor_line("OK").is_none());
        assert!(parse_monitor_line("not-a-timestamp [0 c] \"PING\"").is_none());
        assert!(parse_monitor_line("1751524000.0 no-bracket \"PING\"").is_none());
    }

    #[test]
    fn quoted_args_undo_redis_escapes() {
        let args = parse_quoted_args("\"SET\" \"ke\\\"y\" \"line\\nbreak\" \"\\x41\\t\\\\\"");
        assert_eq!(args, vec!["SET", "ke\"y", "line\nbreak", "A\t\\"]);
    }

    #[test]
    fn quoted_args_never_panic_on_non_ascii_after_hex_escape() {
        // `\x` followed by a multi-byte character used to slice the `&str`
        // mid-character and panic.
        let args = parse_quoted_args("\"a\\x\u{e9}b\" \"\\x4\u{1F600}\"");
        assert_eq!(args, vec!["ax\u{e9}b", "x4\u{1F600}"]);
        // Upper-case hex digits still decode, and a short tail stays literal.
        assert_eq!(parse_quoted_args("\"\\x4A\\xff\""), vec!["J\u{fffd}"]);
        assert_eq!(parse_quoted_args("\"\\x4\""), vec!["x4"]);
    }

    #[test]
    fn quoted_args_handle_empty_and_unterminated() {
        assert_eq!(parse_quoted_args(""), Vec::<String>::new());
        assert_eq!(parse_quoted_args("\"PING\""), vec!["PING"]);
        // Unterminated quote still yields what was read.
        assert_eq!(parse_quoted_args("\"GET\" \"key"), vec!["GET", "key"]);
    }

    #[test]
    fn buffer_drops_oldest_on_overflow_and_counts() {
        let mut buffer = BoundedBuffer::new(3);
        for n in 0..5 {
            buffer.push(n);
        }
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.dropped(), 2);
        assert_eq!(buffer.iter().copied().collect::<Vec<_>>(), vec![2, 3, 4]);
    }

    #[test]
    fn buffer_set_cap_evicts_oldest_and_clear_keeps_the_counter() {
        let mut buffer = BoundedBuffer::new(5);
        for n in 0..5 {
            buffer.push(n);
        }
        buffer.set_cap(2);
        assert_eq!(buffer.iter().copied().collect::<Vec<_>>(), vec![3, 4]);
        assert_eq!(buffer.dropped(), 3);
        buffer.push(5);
        assert_eq!(buffer.iter().copied().collect::<Vec<_>>(), vec![4, 5]);
        buffer.clear();
        assert!(buffer.is_empty());
        assert_eq!(buffer.dropped(), 4);
        buffer.set_cap(0);
        assert_eq!(buffer.cap(), 1, "cap never drops below one");
    }

    #[test]
    fn subscribe_targets_leading_equals_forces_a_literal_channel() {
        assert_eq!(
            parse_subscribe_targets("=orders[eu], =what?, ==x, =, news.*"),
            vec![
                SubscribeTarget::Channel("orders[eu]".into()),
                SubscribeTarget::Channel("what?".into()),
                SubscribeTarget::Channel("=x".into()),
                SubscribeTarget::Pattern("news.*".into()),
            ]
        );
    }

    #[test]
    fn keyspace_events_preset_expands_for_the_database() {
        assert_eq!(key_events_preset(3, ""), "__keyevent@3__:*");
        assert_eq!(
            parse_subscribe_targets(&key_events_preset(3, "")),
            vec![SubscribeTarget::Pattern("__keyevent@3__:*".into())]
        );
        // A key pattern typed first also watches those keys by name.
        assert_eq!(
            key_events_preset(3, "user:*, =session:42"),
            "__keyevent@3__:*, __keyspace@3__:user:*, __keyspace@3__:session:42"
        );
        // Pressing the preset again leaves the expanded text alone.
        let once = key_events_preset(3, "user:*");
        assert_eq!(key_events_preset(3, &once), once);
        assert_eq!(key_events_preset(0, " , "), "__keyevent@0__:*");
    }

    #[test]
    fn keyspace_notifications_split_into_event_and_key() {
        let expired = shape_pubsub_message(b"__keyevent@3__:expired", None, b"user:42", 1);
        assert_eq!(expired.event.as_deref(), Some("expired"));
        assert_eq!(expired.key.as_deref(), Some("user:42"));
        let by_key = shape_pubsub_message(
            b"__keyspace@0__:user:42",
            Some(b"__keyspace@0__:user:*"),
            b"set",
            1,
        );
        assert_eq!(by_key.event.as_deref(), Some("set"));
        assert_eq!(by_key.key.as_deref(), Some("user:42"));
        // A binary key still names its bytes.
        let binary = shape_pubsub_message(b"__keyevent@0__:del", None, &[b'k', 0xff], 1);
        assert_eq!(binary.key.as_deref(), Some("k\\xff"));
        // Ordinary messages and look-alikes stay unsplit.
        for channel in [
            &b"news"[..],
            b"__keyevent@x__:expired",
            b"__keyevent@3__expired",
            b"keyevent@3__:expired",
        ] {
            let plain = shape_pubsub_message(channel, None, b"user:42", 1);
            assert_eq!((plain.event, plain.key), (None, None), "{channel:?}");
        }
    }

    #[test]
    fn display_name_escapes_binary_names_instead_of_hiding_them() {
        assert_eq!(display_name(b"news"), "news");
        assert_eq!(display_name("caf\u{e9}".as_bytes()), "caf\u{e9}");
        assert_eq!(display_name(&[b'c', 0xff, 0x00, b'!']), "c\\xff\\x00!");
        // Two different binary channels stay distinguishable.
        assert_ne!(display_name(&[0xfe]), display_name(&[0xff]));
    }

    #[test]
    fn monitor_frames_classify_lines_errors_and_other_replies() {
        let line = redis::parse_redis_value(b"+1751524000.0 [0 127.0.0.1:1] \"PING\"\r\n")
            .expect("parses");
        assert_eq!(
            monitor_frame(line),
            MonitorFrame::Line("1751524000.0 [0 127.0.0.1:1] \"PING\"".into())
        );
        let error = redis::parse_redis_value(b"-ERR monitor disabled\r\n").expect("parses");
        assert_eq!(
            monitor_frame(error),
            MonitorFrame::ServerError("ERR monitor disabled".into())
        );
        assert_eq!(monitor_frame(redis::Value::Int(1)), MonitorFrame::Other);
    }

    #[test]
    fn filter_matching_is_ascii_case_insensitive() {
        assert!(contains_ignore_ascii_case("\"SET\" \"User:7\"", "user:7"));
        assert!(contains_ignore_ascii_case("abc", ""));
        assert!(!contains_ignore_ascii_case("ab", "abc"));
        assert!(!contains_ignore_ascii_case("\"GET\" \"k\"", "set"));
    }

    #[test]
    fn subscribe_targets_hash_prefix_names_a_sharded_channel() {
        assert_eq!(
            parse_subscribe_targets("#orders, # eu , #, =#literal, news"),
            vec![
                SubscribeTarget::Sharded("orders".into()),
                SubscribeTarget::Sharded("eu".into()),
                SubscribeTarget::Channel("#literal".into()),
                SubscribeTarget::Channel("news".into()),
            ]
        );
    }

    #[test]
    fn subscribe_targets_split_trim_and_classify_globs() {
        let targets = parse_subscribe_targets("news, alerts.* , metrics.[ab], , plain");
        assert_eq!(
            targets,
            vec![
                SubscribeTarget::Channel("news".into()),
                SubscribeTarget::Pattern("alerts.*".into()),
                SubscribeTarget::Pattern("metrics.[ab]".into()),
                SubscribeTarget::Channel("plain".into()),
            ]
        );
        assert!(parse_subscribe_targets(" , ,").is_empty());
        assert_eq!(
            parse_subscribe_targets("what?"),
            vec![SubscribeTarget::Pattern("what?".into())]
        );
    }

    #[test]
    fn pubsub_payload_is_capped_with_its_full_size_recorded() {
        let big = "x".repeat(MAX_STORED_PAYLOAD_BYTES * 3);
        let message = shape_pubsub_message(b"c", None, big.as_bytes(), 1);
        assert_eq!(message.payload.len(), MAX_STORED_PAYLOAD_BYTES);
        assert_eq!(message.payload_len, MAX_STORED_PAYLOAD_BYTES * 3);
        assert!(message.payload_truncated());
        assert!(message.stored_bytes() < MAX_STORED_PAYLOAD_BYTES + 1024);

        // A multi-byte character straddling the cut stays text, minus the
        // split character.
        let mut split = "a".repeat(MAX_STORED_PAYLOAD_BYTES - 1);
        split.push('é'); // 2 bytes: the second falls past the cap
        split.push_str("tail");
        let message = shape_pubsub_message(b"c", None, split.as_bytes(), 1);
        match &message.payload {
            PubSubPayload::Text(text) => assert_eq!(text.len(), MAX_STORED_PAYLOAD_BYTES - 1),
            other => panic!("expected text, got {other:?}"),
        }
        assert!(message.payload_truncated());

        // Binary stays binary, capped the same way.
        let mut bytes = vec![0xffu8; MAX_STORED_PAYLOAD_BYTES + 10];
        bytes[0] = 0x00;
        let message = shape_pubsub_message(b"c", None, &bytes, 1);
        assert!(
            matches!(&message.payload, PubSubPayload::Bytes(b) if b.len() == MAX_STORED_PAYLOAD_BYTES)
        );
        assert_eq!(message.payload_len, MAX_STORED_PAYLOAD_BYTES + 10);

        // Small payloads are untouched and not flagged.
        let small = shape_pubsub_message(b"c", None, b"hi", 1);
        assert!(!small.payload_truncated());
        assert_eq!(small.payload_len, 2);

        // Huge channel names are capped too.
        let long_name = vec![b'n'; MAX_STORED_NAME_BYTES * 4];
        let named = shape_pubsub_message(&long_name, None, b"x", 1);
        assert!(named.channel.len() < MAX_STORED_NAME_BYTES + 64);
        assert!(named.channel.contains("truncated"), "{}", named.channel);
    }

    #[test]
    fn monitor_text_is_capped_with_a_marker() {
        let value = "v".repeat(MAX_STORED_MONITOR_TEXT_BYTES * 2);
        let line = format!("1751524000.0 [0 127.0.0.1:1] \"SET\" \"k\" \"{value}\"");
        let entry = parse_monitor_line(&line).expect("parses");
        assert!(entry.text.len() < MAX_STORED_MONITOR_TEXT_BYTES + 64);
        assert!(
            entry.text.ends_with(&format!(
                "(truncated, {} bytes)",
                MAX_STORED_MONITOR_TEXT_BYTES * 2 + 12
            )),
            "{}",
            &entry.text[entry.text.len() - 40..]
        );
        assert_eq!(entry.command, "SET");
        assert_eq!(utf8_prefix("héllo", 2), "h");
    }

    #[derive(Debug, PartialEq)]
    struct Weighted(usize);
    impl StoredSize for Weighted {
        fn stored_bytes(&self) -> usize {
            self.0
        }
    }

    #[test]
    fn byte_budget_evicts_oldest_but_keeps_the_newest() {
        let mut buffer = BoundedBuffer::with_byte_budget(100, 250);
        buffer.push(Weighted(100));
        buffer.push(Weighted(100));
        assert_eq!(buffer.stored_bytes(), 200);
        buffer.push(Weighted(100)); // 300 > 250: the oldest goes
        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer.stored_bytes(), 200);
        assert_eq!(buffer.dropped(), 1);
        // One entry over the whole budget still stays (it is the newest).
        buffer.push(Weighted(1_000));
        assert_eq!(buffer.iter().collect::<Vec<_>>(), vec![&Weighted(1_000)]);
        assert_eq!(buffer.stored_bytes(), 1_000);
        assert_eq!(buffer.dropped(), 3);
        buffer.clear();
        assert_eq!(buffer.stored_bytes(), 0);
        // The count cap still applies alongside the budget.
        let mut counted = BoundedBuffer::with_byte_budget(2, usize::MAX);
        for n in 0..5 {
            counted.push(Weighted(n));
        }
        assert_eq!(counted.len(), 2);
        assert_eq!(counted.stored_bytes(), 3 + 4);
    }

    #[test]
    fn pubsub_message_shapes_utf8_and_binary_payloads() {
        let text = shape_pubsub_message(b"news", None, b"{\"a\":1}", 1000);
        assert_eq!(text.channel, "news");
        assert_eq!(text.pattern, None);
        assert_eq!(text.payload, PubSubPayload::Text("{\"a\":1}".into()));
        assert_eq!(text.received_millis, 1000);
        let binary = shape_pubsub_message(b"bin", Some(b"b*"), &[0xff, 0x00], 1001);
        assert_eq!(binary.pattern.as_deref(), Some("b*"));
        assert_eq!(binary.payload, PubSubPayload::Bytes(vec![0xff, 0x00]));
        let binary_names = shape_pubsub_message(&[0xff, b'c'], Some(&[0xfe, b'*']), b"x", 1002);
        assert_eq!(binary_names.channel, "\\xffc");
        assert_eq!(binary_names.pattern.as_deref(), Some("\\xfe*"));
    }

    #[test]
    fn profiler_filter_matches_command_client_and_substring() {
        let entry = parse_monitor_line(
            "1751524005.0 [3 10.1.2.3:4000] \"hset\" \"user:7\" \"name\" \"ada\"",
        )
        .expect("parses");
        let open = ProfilerFilter::default();
        assert!(monitor_entry_matches(&entry, &open));
        let by_command = ProfilerFilter {
            command: "HSET".into(),
            ..Default::default()
        };
        assert!(monitor_entry_matches(&entry, &by_command));
        let wrong_command = ProfilerFilter {
            command: "get".into(),
            ..Default::default()
        };
        assert!(!monitor_entry_matches(&entry, &wrong_command));
        let by_client = ProfilerFilter {
            client: "10.1.2".into(),
            ..Default::default()
        };
        assert!(monitor_entry_matches(&entry, &by_client));
        let by_text = ProfilerFilter {
            substring: "user:7".into(),
            ..Default::default()
        };
        assert!(monitor_entry_matches(&entry, &by_text));
        let no_match = ProfilerFilter {
            command: "hset".into(),
            client: "192.168".into(),
            substring: String::new(),
        };
        assert!(!monitor_entry_matches(&entry, &no_match));
    }

    /// Live: MONITOR sees another connection's command; a subscription
    /// receives a published message. Opt-in through `SQLLY_REDIS_URL`
    /// (`redis://127.0.0.1:6380/0`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_monitor_and_pubsub_streams() {
        let Some(profile) = super::super::live_profile_from_env("redis-live-lane") else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        let marker = format!("sqlly-live-{}", now_millis());

        let mut capture = open_monitor(&profile).await.expect("monitor opens");
        let client = super::super::open_client(&profile).await.expect("client");
        let mut set = redis::cmd("SET");
        set.arg(marker.as_str()).arg("1");
        client.run(&set).await.expect("SET runs");
        let seen = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let frame = capture.next_frame().await.expect("stream open");
                if let MonitorFrame::Line(line) = frame
                    && line.contains(&marker)
                {
                    break parse_monitor_line(&line).expect("line parses");
                }
            }
        })
        .await
        .expect("MONITOR saw the SET within 5s");
        assert_eq!(seen.command, "SET");
        drop(capture);

        let targets = parse_subscribe_targets(&format!("{marker}-chan, {marker}-pat.*"));
        let mut listener = open_pubsub(&profile, &targets).await.expect("subscribes");
        let mut publish = redis::cmd("PUBLISH");
        publish
            .arg(format!("{marker}-pat.news"))
            .arg("{\"hello\":1}");
        client.run(&publish).await.expect("PUBLISH runs");
        let message = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let message = listener.next_message().await.expect("stream open");
                if message.channel == format!("{marker}-pat.news") {
                    break message;
                }
            }
        })
        .await
        .expect("message arrived within 5s");
        assert_eq!(
            message.pattern.as_deref(),
            Some(format!("{marker}-pat.*").as_str())
        );
        assert_eq!(message.payload, PubSubPayload::Text("{\"hello\":1}".into()));
        drop(listener);
    }

    /// Live: the Key events preset on database 12 sees a key set with
    /// `EX 1` expire as an `expired` row naming the key. Turns key-event
    /// notifications on when the fixture has them off (`CONFIG SET
    /// notify-keyspace-events KEA`, the same statement the viewer offers)
    /// and leaves them on, since other suites share the server.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_keyspace_expired_event_row() {
        let Some(mut profile) = super::super::live_profile_from_env("redis-live-keyspace") else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        profile.database = Some("12".into());
        let client = super::super::open_client(&profile).await.expect("client");
        let mut get = redis::cmd("CONFIG");
        get.arg("GET").arg("notify-keyspace-events");
        let flags = match client.run(&get).await.expect("CONFIG GET runs") {
            redis::Value::Array(items) => items
                .get(1)
                .and_then(|v| match v {
                    redis::Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
                    _ => None,
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        if !(flags.contains('E') && flags.contains('A')) {
            let mut set = redis::cmd("CONFIG");
            set.arg("SET").arg("notify-keyspace-events").arg("KEA");
            client.run(&set).await.expect("CONFIG SET runs");
        }
        let targets = parse_subscribe_targets(&key_events_preset(12, ""));
        assert_eq!(
            targets,
            vec![SubscribeTarget::Pattern("__keyevent@12__:*".into())]
        );
        let mut listener = open_pubsub(&profile, &targets).await.expect("subscribes");
        let key = format!("wsh:expiring:{}", now_millis());
        let mut set = redis::cmd("SET");
        set.arg(key.as_str()).arg("1").arg("EX").arg(1);
        client.run(&set).await.expect("SET EX runs");
        // Expiry is lazy-plus-sampled: keep touching the key so the active
        // expire cycle or the access path fires the event promptly.
        let message = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let next = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    listener.next_message(),
                )
                .await;
                match next {
                    Ok(Some(message))
                        if message.event.as_deref() == Some("expired")
                            && message.key.as_deref() == Some(key.as_str()) =>
                    {
                        break message;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => panic!("subscription ended"),
                    Err(_) => {
                        let mut exists = redis::cmd("EXISTS");
                        exists.arg(key.as_str());
                        client.run(&exists).await.expect("EXISTS runs");
                    }
                }
            }
        })
        .await
        .expect("expired event arrived within 15s");
        assert_eq!(message.channel, "__keyevent@12__:expired");
        assert_eq!(message.pattern.as_deref(), Some("__keyevent@12__:*"));
        drop(listener);
    }
}
