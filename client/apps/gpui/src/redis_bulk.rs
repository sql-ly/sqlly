//! Pure orchestration for the Redis bulk-actions dialog: batched command
//! construction (`UNLINK` / `EXPIRE` / `PERSIST`), dry-run row parsing that
//! preserves binary key names, and the per-line error accounting the
//! driver's semantics make possible.
//!
//! The driver runs a script's lines in order and aborts the script at the
//! first server error, prefixed `line N:` (and, by the time it reaches a
//! tool dialog, by the error kind's own text: `Query execution error: line
//! N: …`). Chunked application turns that into exact accounting: lines
//! before N ran, line N failed and is recorded, and the remainder is
//! re-issued as the next chunk — every line is attempted at most once, and
//! every failure names its line. `MULTI … EXEC` blocks never straddle a
//! chunk (each chunk is a fresh connection), and a failure inside one
//! skips the rest of the discarded transaction instead of replaying it
//! outside `MULTI`. Nothing in here talks to a server; the dialog feeds
//! real query outcomes in.
//!
//! The import's safety review lives here too: every line is classified
//! with the same command allowlist the editor's run gate uses, and the
//! destructive or server-administration lines are listed by line number so
//! the dialog can demand a typed confirmation before any of them runs.

use sqlly_core::CellValue;
use sqlly_intel::query::safety::ProductionDecision;
use sqlly_intel::query::safety::redis::{
    CommandAccess, RedisCommand, command_access, quote_argument,
};

/// Keys one `UNLINK` line carries. The command takes many keys per call, so
/// a batch is one atomic line — and the driver streams one result set per
/// line, which is what the dialog reports as progress.
pub const UNLINK_BATCH: usize = 500;
/// Command lines per `EXPIRE`/`PERSIST` or import chunk: small enough that
/// a mid-chunk failure leaves little to resume, large enough that a big
/// file does not need hundreds of round trips.
pub const LINE_CHUNK: usize = 200;
/// Keys a bulk dry run scans at most. The apply step only ever touches what
/// the dry run listed, so this is also the most one apply can affect; the
/// dialog says so when the scan capped.
pub const BULK_KEY_LIMIT: usize = 25_000;

/// Keys one dry-run page asks the walk for. The dry run pages its `SCAN` so
/// a Stop between pages lands within one page's worth of work, while the
/// whole [`BULK_KEY_LIMIT`] still takes only a handful of round trips.
pub const DRY_RUN_PAGE: usize = 5_000;

/// The bulk dialog's dry run: the same bounded, non-blocking `SCAN` the key
/// browser lists with, driven one page at a time so the user can stop it.
/// Nothing here talks to a server; the dialog runs [`DryRun::next_command`]
/// and feeds the rows back through [`DryRun::absorb`]. A stopped or capped
/// run is **partial** and says so: the keys it holds are exactly what the
/// apply may touch, never more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRun {
    pattern: String,
    limit: usize,
    /// The cursor the next page continues from (`"0"` before the first).
    cursor: String,
    /// The walk reported a cursor it can continue from. A driver without
    /// the cursor column leaves a capped walk nowhere to go, which counts
    /// as capped, not complete.
    can_continue: bool,
    /// Every key matched so far, byte-exact.
    pub keys: Vec<Vec<u8>>,
    /// The walk came back to cursor 0: the count is the whole truth.
    pub complete: bool,
    /// The user stopped the walk between pages.
    pub stopped: bool,
    /// The walk ran on a Redis Cluster (its rows carried the `masters`
    /// column): deletes must be batched by hash slot.
    pub cluster: bool,
}

impl DryRun {
    pub fn new(pattern: &str, limit: usize) -> Self {
        Self {
            pattern: pattern.to_string(),
            limit: limit.max(1),
            cursor: "0".to_string(),
            can_continue: true,
            keys: Vec::new(),
            complete: false,
            stopped: false,
            cluster: false,
        }
    }

    /// The walk hit [`Self::limit`] before finishing.
    pub fn capped(&self) -> bool {
        !self.complete && !self.stopped && (self.keys.len() >= self.limit || !self.can_continue)
    }

    /// The count on screen is not the whole keyspace's match count.
    pub fn is_partial(&self) -> bool {
        !self.complete
    }

    /// The command for the next page, or `None` when the walk is complete,
    /// stopped, or at its budget.
    pub fn next_command(&self) -> Option<String> {
        if self.complete || self.stopped || self.keys.len() >= self.limit || !self.can_continue {
            return None;
        }
        let page = DRY_RUN_PAGE.min(self.limit - self.keys.len());
        Some(crate::schema_tree::redis::keys_command_from(
            &self.pattern,
            page,
            &self.cursor,
        ))
    }

    /// Fold one page's rows in. Keys are byte-exact; the page's `complete`
    /// flag ends the walk, otherwise its cursor is where the next page
    /// starts. The list is cut at the limit so the apply never exceeds what
    /// the dialog promised, and the cut is reported as capped.
    pub fn absorb(&mut self, rows: &[Vec<CellValue>]) {
        let (keys, complete) = parse_key_cells(rows);
        let cursor = crate::schema_tree::redis::listing_cursor(rows);
        if crate::schema_tree::redis::listing_masters(rows).is_some() {
            self.cluster = true;
        }
        self.keys.extend(keys);
        if self.keys.len() > self.limit {
            self.keys.truncate(self.limit);
            self.can_continue = false;
        }
        if complete {
            self.complete = true;
        } else {
            match cursor {
                Some(cursor) => self.cursor = cursor,
                None => self.can_continue = false,
            }
        }
    }

    /// Stop between pages: the keys seen so far stay, the walk does not
    /// continue, and the count is labelled partial.
    pub fn stop(&mut self) {
        if !self.complete {
            self.stopped = true;
        }
    }

    /// The one-line match count, honest about what it covers.
    pub fn status(&self) -> String {
        let count = self.keys.len();
        let (key, arguments) = if self.stopped {
            (
                "explorer.redis_bulk.matches_stopped",
                vec![("count", count.to_string())],
            )
        } else if self.capped() {
            (
                "explorer.redis_bulk.matches_capped",
                vec![
                    ("count", count.to_string()),
                    ("limit", self.limit.to_string()),
                ],
            )
        } else if count == 0 {
            ("explorer.redis_bulk.matches_empty", Vec::new())
        } else {
            (
                "explorer.redis_bulk.matches",
                vec![("count", count.to_string())],
            )
        };
        let arguments = arguments
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect::<Vec<_>>();
        crate::localization::format(key, &arguments)
    }
}

/// The smallest batch the pacer issues: small enough to land in well under
/// a second on any server that answers at all.
pub const PACER_FLOOR: usize = 50;
/// How long one batch should take at most. Stop is honoured between
/// batches, so this is also how long a Stop can wait.
pub const PACER_TARGET: std::time::Duration = std::time::Duration::from_secs(1);

/// Sizes the apply's batches by how long the last one took, so a Stop —
/// which lets the batch in flight finish — lands within about a second: a
/// batch slower than [`PACER_TARGET`] halves the next (down to
/// [`PACER_FLOOR`]), a batch done in under a quarter of it doubles the next
/// (up to the ceiling the apply started with).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchPacer {
    size: usize,
    ceiling: usize,
}

impl BatchPacer {
    pub fn new(ceiling: usize) -> Self {
        let ceiling = ceiling.max(PACER_FLOOR);
        Self {
            size: ceiling,
            ceiling,
        }
    }

    /// The next batch's size.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Fold in how long the batch just finished took.
    pub fn record(&mut self, elapsed: std::time::Duration) {
        if elapsed > PACER_TARGET {
            self.size = (self.size / 2).max(PACER_FLOOR);
        } else if elapsed * 4 < PACER_TARGET {
            self.size = (self.size * 2).min(self.ceiling);
        }
    }
}

impl Default for BatchPacer {
    fn default() -> Self {
        Self::new(UNLINK_BATCH)
    }
}

/// Key names from `SQLLY.KEYS` rows, byte-exact: a `String` cell is valid
/// UTF-8 by construction (the driver only builds it from valid bulk
/// strings), a `Bytes` cell is the raw name. Lossy text would make a bulk
/// `UNLINK` target the wrong keys, so lossy conversion lives nowhere here.
/// The second half of the tuple is the walk's `complete` flag.
pub fn parse_key_cells(rows: &[Vec<CellValue>]) -> (Vec<Vec<u8>>, bool) {
    let mut complete = true;
    let keys = rows
        .iter()
        .filter_map(|row| {
            if let Some(CellValue::Bool(false)) = row.get(3) {
                complete = false;
            }
            match row.first()? {
                CellValue::String(s) => Some(s.clone().into_bytes()),
                CellValue::Bytes(b) => Some(b.clone()),
                _ => None,
            }
        })
        .collect();
    (keys, complete)
}

/// Order a cluster dry run's keys by hash slot, so every `UNLINK` batch
/// can stay inside one slot (a cluster refuses a multi-key command whose
/// keys span slots). Stable: keys of one slot keep their walk order.
pub fn order_by_slot(keys: &mut [Vec<u8>]) {
    keys.sort_by_key(|key| sqlly_engines::redis::cluster::key_slot(key));
}

/// Where the next `UNLINK` batch from `start` ends: `size` keys on
/// (clamped to the list), and on a cluster also no further than the last
/// key sharing the first key's hash slot.
pub fn batch_end(keys: &[Vec<u8>], start: usize, size: usize, cluster: bool) -> usize {
    let end = (start + size.max(1)).min(keys.len());
    if !cluster || start >= end {
        return end;
    }
    let slot = sqlly_engines::redis::cluster::key_slot(&keys[start]);
    keys[start..end]
        .iter()
        .position(|key| sqlly_engines::redis::cluster::key_slot(key) != slot)
        .map_or(end, |offset| start + offset)
}

/// One `UNLINK` line for a batch of keys, quoting binary-safe.
pub fn unlink_line(keys: &[Vec<u8>]) -> String {
    let mut line = String::from("UNLINK");
    for key in keys {
        line.push(' ');
        line.push_str(&quote_argument(key));
    }
    line
}

/// The batch-TTL apply commands: one binary-safe `EXPIRE key seconds` (or
/// `PERSIST key` when clearing) per key, in order.
pub fn ttl_commands(keys: &[Vec<u8>], seconds: i64, persist: bool) -> Vec<RedisCommand> {
    keys.iter()
        .map(|key| RedisCommand {
            line: 0,
            args: if persist {
                vec![b"PERSIST".to_vec(), key.clone()]
            } else {
                vec![
                    b"EXPIRE".to_vec(),
                    key.clone(),
                    seconds.to_string().into_bytes(),
                ]
            },
        })
        .collect()
}

/// The error-kind prefixes a per-line driver error can carry once it is
/// rendered (`StudioError`'s `Display`): a server `-ERR` reply is a query
/// execution error, an ACL `NOPERM` a permission error. Anything else
/// (connection lost, cancelled, a script-wide policy refusal) never names
/// the one line that failed and must not be read as if it did.
const LINE_ERROR_KINDS: [&str; 2] = ["Query execution error: ", "Permission denied: "];

/// Split a driver error into the line it names and the server's own text.
/// The `line N:` marker must lead the message (after at most one known
/// error-kind prefix): a policy refusal such as `Permission denied:
/// connection policy allows read-only commands only; line 5: …` refused the
/// whole script before anything ran, so it names no failed line.
pub fn split_line_error(error: &str) -> Option<(usize, &str)> {
    let body = LINE_ERROR_KINDS
        .iter()
        .find_map(|kind| error.strip_prefix(kind))
        .unwrap_or(error);
    let rest = body.strip_prefix("line ")?;
    let (number, text) = rest.split_once(':')?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((number.parse().ok()?, text.trim()))
}

/// The line number a driver error names (`… line N: …`), when present.
pub fn parse_line_error(error: &str) -> Option<usize> {
    split_line_error(error).map(|(line, _)| line)
}

/// The error text with its kind and `line N: ` prefixes stripped, for
/// display next to the failed line itself.
pub fn strip_line_prefix(error: &str) -> &str {
    split_line_error(error).map_or(error, |(_, text)| text)
}

/// Decode an import file: UTF-8 only (a leading byte-order mark is
/// dropped). Invalid bytes are refused with the line they sit on — a lossy
/// decode would silently rewrite binary values as U+FFFD before writing
/// them to the server. Binary values belong in `"\xHH"` escapes.
pub fn decode_import(bytes: &[u8]) -> Result<&str, String> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    std::str::from_utf8(bytes).map_err(|error| {
        let line = bytes[..error.valid_up_to()]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
            + 1;
        crate::localization::format(
            "explorer.redis_bulk.invalid_utf8",
            &[("line", &line.to_string())],
        )
    })
}

/// Why a write-capable key dialog must not write at all on this
/// connection: the profile's read-only policy, or a locked production
/// guard. `None` when writes may proceed (still subject to the dialog's
/// own confirmations).
pub fn write_block_reason(
    read_only_profile: bool,
    production: ProductionDecision,
) -> Option<String> {
    if read_only_profile {
        return Some(crate::localization::text(
            "explorer.redis_bulk.read_only_connection",
        ));
    }
    if production == ProductionDecision::ReadOnly {
        return Some(crate::localization::text(
            "explorer.redis_bulk.production_read_only",
        ));
    }
    None
}

/// Server-administration command families: never plain data changes,
/// flagged for review whatever their allowlist access is.
const ADMIN_COMMANDS: &[&str] = &[
    "ACL",
    "BGREWRITEAOF",
    "BGSAVE",
    "CLIENT",
    "CLUSTER",
    "CONFIG",
    "DEBUG",
    "FAILOVER",
    "FUNCTION",
    "LATENCY",
    "MODULE",
    "REPLICAOF",
    "SAVE",
    "SCRIPT",
    "SHUTDOWN",
    "SLAVEOF",
    "SLOWLOG",
];

/// One import line the safety review flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlaggedCommand {
    pub line: usize,
    pub command: String,
}

/// What an import file would do, classified before anything runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReview {
    pub total: usize,
    /// Lines that write at all (the editor gate's "data change").
    pub writes: usize,
    /// Destructive lines (`FLUSHALL`, `DEL`, `CONFIG SET`, …) and any
    /// non-read-only server-administration line, in file order.
    pub flagged: Vec<FlaggedCommand>,
}

impl ImportReview {
    pub fn of(commands: &[RedisCommand]) -> Self {
        let mut review = ImportReview {
            total: commands.len(),
            ..Default::default()
        };
        for command in commands {
            let access = command_access(command);
            if access.is_read_only() {
                continue;
            }
            review.writes += 1;
            let admin = ADMIN_COMMANDS.contains(&command.effective_name().as_str());
            if access == CommandAccess::Destructive || admin {
                review.flagged.push(FlaggedCommand {
                    line: command.line,
                    command: command.to_cli_line(),
                });
            }
        }
        review
    }
}

/// The phrase an import with flagged lines (or one the connection's safety
/// settings want confirmed) must have typed before it runs.
pub const IMPORT_CONFIRM_PHRASE: &str = "RUN ALL";

/// The logical database a consumed script prefix leaves its connection on:
/// the last `SELECT`'s argument that parses. Fresh connections start on the
/// profile's database, so a resume chunk must re-`SELECT` what an earlier
/// chunk already switched to.
pub fn selected_database(commands: &[RedisCommand]) -> Option<i64> {
    commands.iter().rev().find_map(|command| {
        if command.name() == "SELECT" {
            command.arg_text(1).trim().parse::<i64>().ok()
        } else {
            None
        }
    })
}

/// One failed line of a chunked run, named the way the user wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineFailure {
    /// The line number in the user's file (import) or 0 for generated
    /// commands, whose `command` text already says what ran.
    pub line: usize,
    pub command: String,
    pub error: String,
}

/// Running tally of a chunked apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkOutcome {
    pub ok: usize,
    pub failed: Vec<LineFailure>,
    /// Lines never run because the transaction they belonged to was
    /// discarded by a failure inside it.
    pub skipped: usize,
}

/// `MULTI` opens a transaction block; `EXEC` / `DISCARD` closes it.
fn opens_block(command: &RedisCommand) -> bool {
    command.name() == "MULTI"
}

fn closes_block(command: &RedisCommand) -> bool {
    matches!(command.name().as_str(), "EXEC" | "DISCARD")
}

/// Index of the `MULTI` whose block is still open just before `index`, if
/// any (`commands[..index]` has a `MULTI` with no `EXEC`/`DISCARD` after it).
fn open_block_start(commands: &[RedisCommand], index: usize) -> Option<usize> {
    let mut open = None;
    for (i, command) in commands[..index.min(commands.len())].iter().enumerate() {
        if opens_block(command) {
            open = Some(i);
        } else if closes_block(command) {
            open = None;
        }
    }
    open
}

/// Index just past the `EXEC`/`DISCARD` closing the block open at `from`
/// (or the end of the file when it never closes).
fn block_end(commands: &[RedisCommand], from: usize) -> usize {
    commands[from..]
        .iter()
        .position(closes_block)
        .map_or(commands.len(), |offset| from + offset + 1)
}

/// One chunk of a line-wise apply: the script to run and the user commands
/// it covers, in order, so the outcome maps back to file lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineChunk {
    pub script: String,
    pub commands: Vec<RedisCommand>,
    /// The script leads with a `SELECT` re-establishing an earlier chunk's
    /// database switch, shifting the driver's `line N` by one.
    pub prepended_select: bool,
}

/// What a chunk's run came back with.
pub enum ChunkResult {
    /// Every line ran.
    Ok,
    /// The driver aborted the chunk; its `line N:` prefix says where.
    Failed(String),
    /// The run never reached the server (connection lost, cancelled): no
    /// line in the chunk can be accounted for, so the apply stops.
    Fatal(String),
}

/// A line-wise apply in flight: parsed commands, a cursor, and the tally.
/// `SELECT` context is preserved across chunks by re-issuing the last
/// switch at the head of every resume script.
#[derive(Debug, Clone, Default)]
pub struct LineApply {
    commands: Vec<RedisCommand>,
    next: usize,
    pub outcome: ChunkOutcome,
    /// Chunk sizing from observed latency, starting at [`LINE_CHUNK`].
    pacer: BatchPacer,
}

impl LineApply {
    pub fn new(commands: Vec<RedisCommand>) -> Self {
        Self {
            commands,
            next: 0,
            outcome: ChunkOutcome::default(),
            pacer: BatchPacer::new(LINE_CHUNK),
        }
    }

    /// How long the chunk just finished took, so the next one is sized to
    /// land within [`PACER_TARGET`].
    pub fn record_chunk(&mut self, elapsed: std::time::Duration) {
        self.pacer.record(elapsed);
    }

    pub fn total(&self) -> usize {
        self.commands.len()
    }

    /// Commands accounted for so far (ok + failed + skipped).
    pub fn processed(&self) -> usize {
        self.outcome.ok + self.outcome.failed.len() + self.outcome.skipped
    }

    /// Where the next chunk ends: the pacer's chunk size on (at most
    /// [`LINE_CHUNK`] lines), moved so a
    /// `MULTI … EXEC` block never straddles two chunks (two connections).
    /// A block that would be cut is deferred whole to the next chunk; a
    /// block longer than a chunk starting at the chunk's head is taken
    /// whole.
    fn chunk_end(&self) -> usize {
        let len = self.commands.len();
        let end = (self.next + self.pacer.size()).min(len);
        if end == len {
            return end;
        }
        match open_block_start(&self.commands, end) {
            Some(start) if start > self.next => start,
            Some(start) => block_end(&self.commands, start),
            None => end,
        }
    }

    pub fn is_done(&self) -> bool {
        self.next >= self.commands.len()
    }

    /// The next chunk's script, or `None` when the apply is done.
    pub fn next_chunk(&self) -> Option<LineChunk> {
        let commands: Vec<RedisCommand> = self.commands.get(self.next..self.chunk_end())?.to_vec();
        if commands.is_empty() {
            return None;
        }
        let db = selected_database(&self.commands[..self.next]);
        let mut script = String::new();
        let prepended_select = db.is_some();
        if let Some(db) = db {
            script.push_str(&format!("SELECT {db}\n"));
        }
        for (i, command) in commands.iter().enumerate() {
            if i > 0 {
                script.push('\n');
            }
            script.push_str(&command.to_cli_line());
        }
        Some(LineChunk {
            script,
            commands,
            prepended_select,
        })
    }

    /// Fold a finished chunk's result into the tally. A mid-chunk failure
    /// records the failed line and advances past it, so the following call
    /// to [`Self::next_chunk`] resumes with the remainder.
    pub fn apply(&mut self, chunk: &LineChunk, result: ChunkResult) {
        match result {
            ChunkResult::Ok => {
                self.outcome.ok += chunk.commands.len();
                self.next += chunk.commands.len();
            }
            ChunkResult::Failed(error) => {
                let shift = usize::from(chunk.prepended_select);
                let index = parse_line_error(&error)
                    .and_then(|line| line.checked_sub(1 + shift))
                    .filter(|index| *index < chunk.commands.len());
                let Some(index) = index else {
                    // The error named no line in this chunk (connection
                    // trouble surfacing as a query error): nothing in it can
                    // be accounted for — stop like a fatal result.
                    self.next = self.commands.len();
                    self.outcome.failed.push(LineFailure {
                        line: 0,
                        command: String::new(),
                        error,
                    });
                    return;
                };
                let failed = &chunk.commands[index];
                self.outcome.failed.push(LineFailure {
                    line: failed.line,
                    command: failed.to_cli_line(),
                    error: strip_line_prefix(&error).to_string(),
                });
                let failed_at = self.next + index;
                // A failure inside `MULTI … EXEC` discards the transaction:
                // the lines queued before it never ran, and the rest of the
                // block must not run outside the transaction on the next
                // chunk's fresh connection. Skip to past its EXEC.
                let block = if opens_block(failed) {
                    Some(failed_at)
                } else {
                    open_block_start(&self.commands, failed_at).filter(|start| *start >= self.next)
                };
                match block {
                    Some(start) => {
                        let resume = if closes_block(failed) {
                            // EXEC itself failed (EXECABORT): nothing queued ran.
                            failed_at + 1
                        } else {
                            block_end(&self.commands, failed_at + 1)
                        };
                        self.outcome.ok += start - self.next;
                        self.outcome.skipped += (failed_at - start) + (resume - failed_at - 1);
                        self.next = resume;
                    }
                    None => {
                        self.outcome.ok += index;
                        self.next = failed_at + 1;
                    }
                }
            }
            ChunkResult::Fatal(error) => {
                self.next = self.commands.len();
                self.outcome.failed.push(LineFailure {
                    line: 0,
                    command: String::new(),
                    error,
                });
            }
        }
    }

    /// One-line summary: `48 commands ok, 2 failed` / `12 of 50 commands
    /// applied (stopped early), 1 failed`.
    pub fn summary(&self, verb: &str) -> String {
        let complete = self.is_done()
            && self.next == self.commands.len()
            && self.processed() == self.commands.len();
        let failed = self.outcome.failed.len();
        let skipped = self.outcome.skipped;
        let key = match (complete, failed > 0, skipped > 0) {
            (true, false, false) => "explorer.redis_bulk.summary_complete",
            (true, true, false) => "explorer.redis_bulk.summary_complete_failed",
            (true, false, true) => "explorer.redis_bulk.summary_complete_skipped",
            (true, true, true) => "explorer.redis_bulk.summary_complete_failed_skipped",
            (false, false, false) => "explorer.redis_bulk.summary_stopped",
            (false, true, false) => "explorer.redis_bulk.summary_stopped_failed",
            (false, false, true) => "explorer.redis_bulk.summary_stopped_skipped",
            (false, true, true) => "explorer.redis_bulk.summary_stopped_failed_skipped",
        };
        let noun = crate::localization::text(if verb == "keys" {
            "explorer.redis_bulk.noun_keys"
        } else {
            "explorer.redis_bulk.noun_commands"
        });
        let mut arguments = vec![("ok", self.outcome.ok.to_string()), ("noun", noun)];
        if !complete {
            arguments.push(("total", self.commands.len().to_string()));
        }
        if failed > 0 {
            arguments.push(("failed", failed.to_string()));
        }
        if skipped > 0 {
            arguments.push(("skipped", skipped.to_string()));
        }
        let arguments = arguments
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect::<Vec<_>>();
        crate::localization::format(key, &arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::query::safety::redis::parse_script;

    fn keys(names: &[&str]) -> Vec<Vec<u8>> {
        names.iter().map(|n| n.as_bytes().to_vec()).collect()
    }

    #[test]
    fn key_cells_preserve_binary_names_and_read_completeness() {
        let rows = vec![
            vec![
                CellValue::String("user:1".into()),
                CellValue::String("hash".into()),
                CellValue::Null,
                CellValue::Bool(true),
            ],
            vec![
                CellValue::Bytes(vec![0xff, 0x00]),
                CellValue::String("string".into()),
                CellValue::Null,
                CellValue::Bool(false),
            ],
        ];
        let (keys, complete) = parse_key_cells(&rows);
        assert_eq!(keys, vec![b"user:1".to_vec(), vec![0xff, 0x00]]);
        assert!(!complete);
        let (none, complete) = parse_key_cells(&[]);
        assert!(none.is_empty() && complete);
    }

    #[test]
    fn cluster_unlink_batches_never_span_a_hash_slot() {
        let mut keys: Vec<Vec<u8>> = (0..2_000)
            .map(|i| format!("session:{i}").into_bytes())
            .collect();
        keys.extend((0..700).map(|i| format!("{{user:7}}:k{i}").into_bytes()));
        order_by_slot(&mut keys);
        let slot = sqlly_engines::redis::cluster::key_slot;
        let mut start = 0;
        let mut batches = 0;
        while start < keys.len() {
            let end = batch_end(&keys, start, UNLINK_BATCH, true);
            assert!(end > start && end - start <= UNLINK_BATCH);
            let first = slot(&keys[start]);
            assert!(
                keys[start..end].iter().all(|k| slot(k) == first),
                "batch {batches} spans slots"
            );
            start = end;
            batches += 1;
        }
        // The 700 hash-tagged keys share a slot: two batches of at most 500.
        assert!(batches >= 2);
        // Standalone batching is unchanged: fixed-size runs in walk order.
        let plain: Vec<Vec<u8>> = (0..1_200).map(|i| format!("k{i}").into_bytes()).collect();
        assert_eq!(batch_end(&plain, 0, UNLINK_BATCH, false), 500);
        assert_eq!(batch_end(&plain, 1_000, UNLINK_BATCH, false), 1_200);
        let mut run = DryRun::new("k*", 10);
        assert!(!run.cluster);
        run.absorb(&[vec![
            CellValue::String("k1".into()),
            CellValue::String("string".into()),
            CellValue::Null,
            CellValue::Bool(true),
            CellValue::String("0".into()),
            CellValue::I64(3),
        ]]);
        assert!(run.cluster, "the masters column marks a cluster walk");
    }

    #[test]
    fn unlink_plan_batches_and_quotes() {
        assert_eq!(unlink_line(&keys(&["a", "b c"])), "UNLINK a \"b c\"");
        assert_eq!(
            unlink_line(&[vec![0xff]]),
            "UNLINK \"\\xff\"",
            "binary names quote, never lossy"
        );
        let many: Vec<Vec<u8>> = (0..UNLINK_BATCH * 2 + 7)
            .map(|i| format!("k{i}").into_bytes())
            .collect();
        let plan: Vec<String> = many.chunks(UNLINK_BATCH).map(unlink_line).collect();
        assert_eq!(plan.len(), 3);
        assert!(plan[0].starts_with("UNLINK k0 "));
        assert!(plan[0].ends_with(" k499"));
        assert!(plan[1].starts_with("UNLINK k500 "));
        assert!(plan[2].starts_with("UNLINK k1000 "));
        assert!(plan[2].ends_with(" k1006"));
    }

    #[test]
    fn ttl_commands_are_one_binary_safe_command_per_key() {
        let mut binary = keys(&["a"]);
        binary.push(vec![0xff, b' ', 0x00]);
        let expire = ttl_commands(&binary, 60, false);
        assert_eq!(expire.len(), 2);
        assert_eq!(
            expire[0].args,
            vec![b"EXPIRE".to_vec(), b"a".to_vec(), b"60".to_vec()]
        );
        assert_eq!(
            expire[1].args[1],
            vec![0xff, b' ', 0x00],
            "key bytes kept exactly"
        );
        let persist = ttl_commands(&keys(&["a"]), 60, true);
        assert_eq!(persist[0].args, vec![b"PERSIST".to_vec(), b"a".to_vec()]);
    }

    #[test]
    fn line_errors_parse_and_strip() {
        assert_eq!(parse_line_error("line 12: WRONGTYPE nope"), Some(12));
        assert_eq!(parse_line_error("WRONGTYPE nope"), None);
        assert_eq!(parse_line_error("line x: nope"), None);
        assert_eq!(parse_line_error("line : nope"), None);
        assert_eq!(strip_line_prefix("line 3: ERR bad"), "ERR bad");
        assert_eq!(strip_line_prefix("ERR bad"), "ERR bad");
    }

    /// The exact text a failing line reaches the dialog as: the driver's
    /// `line N:` wrapped in `StudioError`, rendered by `Display` into the
    /// query event the tool runner collects.
    #[test]
    fn line_errors_parse_through_the_rendered_studio_error() {
        use sqlly_core::StudioError;
        let server = "WRONGTYPE Operation against a key holding the wrong kind of value";
        let rendered = StudioError::QueryExecution(format!("line {}: {server}", 7)).to_string();
        assert!(
            rendered.starts_with("Query execution error: "),
            "{rendered}"
        );
        assert_eq!(parse_line_error(&rendered), Some(7));
        assert_eq!(strip_line_prefix(&rendered), server);
        // An ACL denial on one line is that line's failure too.
        let noperm = StudioError::PermissionDenied(
            "line 2: NOPERM this user has no permissions to run the 'set' command".into(),
        )
        .to_string();
        assert_eq!(parse_line_error(&noperm), Some(2));
        assert!(strip_line_prefix(&noperm).starts_with("NOPERM"));
        // A script-wide policy refusal mentions a line but ran nothing: it
        // must not be read as that one line failing.
        let policy = StudioError::PermissionDenied(
            "connection policy allows read-only commands only; line 5: SET writes or changes the server".into(),
        )
        .to_string();
        assert_eq!(parse_line_error(&policy), None);
        // Connection trouble names no line.
        let lost = StudioError::Connection("line 3: reset".into()).to_string();
        assert_eq!(parse_line_error(&lost), None);
    }

    #[test]
    fn a_rendered_line_error_resumes_the_apply_instead_of_stopping_it() {
        use sqlly_core::StudioError;
        let commands = parse_script("SET a 1\nLPUSH a x\nSET c 3").unwrap();
        let mut apply = LineApply::new(commands);
        let chunk = apply.next_chunk().unwrap();
        let error = StudioError::QueryExecution("line 2: WRONGTYPE wrong kind".into()).to_string();
        apply.apply(&chunk, ChunkResult::Failed(error));
        assert!(!apply.is_done(), "the file continues after a failed line");
        assert_eq!(apply.outcome.ok, 1);
        assert_eq!(apply.outcome.failed[0].line, 2);
        assert_eq!(apply.outcome.failed[0].error, "WRONGTYPE wrong kind");
        let chunk = apply.next_chunk().unwrap();
        assert_eq!(chunk.commands[0].line, 3);
        apply.apply(&chunk, ChunkResult::Ok);
        assert_eq!(apply.summary("commands"), "2 commands ok, 1 failed");
    }

    #[test]
    fn transaction_blocks_never_straddle_chunks() {
        // 195 plain lines, then a 10-line MULTI block: the block would cross
        // the 200-line boundary, so the first chunk stops before MULTI.
        let mut lines: Vec<String> = (1..=195).map(|i| format!("SET k{i} v")).collect();
        lines.push("MULTI".into());
        lines.extend((0..8).map(|i| format!("INCR t{i}")));
        lines.push("EXEC".into());
        lines.push("SET after 1".into());
        let commands = parse_script(&lines.join("\n")).unwrap();
        let mut apply = LineApply::new(commands);
        let chunk = apply.next_chunk().unwrap();
        assert_eq!(chunk.commands.len(), 195);
        apply.apply(&chunk, ChunkResult::Ok);
        let chunk = apply.next_chunk().unwrap();
        assert_eq!(chunk.commands[0].name(), "MULTI");
        assert_eq!(chunk.commands.len(), 11);

        // A block longer than a chunk, starting at the chunk head, is taken whole.
        let mut long = vec!["MULTI".to_string()];
        long.extend((0..(LINE_CHUNK + 50)).map(|i| format!("INCR t{i}")));
        long.push("EXEC".into());
        long.push("GET x".into());
        let apply = LineApply::new(parse_script(&long.join("\n")).unwrap());
        let chunk = apply.next_chunk().unwrap();
        assert_eq!(chunk.commands.len(), LINE_CHUNK + 52);
        assert_eq!(chunk.commands.last().unwrap().name(), "EXEC");
    }

    #[test]
    fn a_failure_inside_a_transaction_skips_the_discarded_block() {
        let script = "SET a 1\nMULTI\nINCR n\nBADCMD\nINCR m\nEXEC\nSET z 1";
        let mut apply = LineApply::new(parse_script(script).unwrap());
        let chunk = apply.next_chunk().unwrap();
        apply.apply(
            &chunk,
            ChunkResult::Failed("Query execution error: line 4: ERR unknown command".into()),
        );
        // SET a ran; MULTI + INCR n were queued then discarded; INCR m and
        // EXEC must not replay outside the transaction.
        assert_eq!(apply.outcome.ok, 1);
        assert_eq!(apply.outcome.failed.len(), 1);
        assert_eq!(apply.outcome.failed[0].line, 4);
        assert_eq!(apply.outcome.skipped, 4);
        let chunk = apply.next_chunk().unwrap();
        assert_eq!(chunk.commands.len(), 1);
        assert_eq!(chunk.commands[0].line, 7);
        apply.apply(&chunk, ChunkResult::Ok);
        assert!(apply.is_done());
        assert_eq!(apply.processed(), 7);
        assert_eq!(
            apply.summary("commands"),
            "2 commands ok, 1 failed, 4 skipped (discarded transaction)"
        );

        // EXECABORT at EXEC: the queued lines never ran; resume after EXEC.
        let mut apply = LineApply::new(parse_script("MULTI\nINCR n\nEXEC\nGET n").unwrap());
        let chunk = apply.next_chunk().unwrap();
        apply.apply(
            &chunk,
            ChunkResult::Failed("Query execution error: line 3: EXECABORT discarded".into()),
        );
        assert_eq!(apply.outcome.ok, 0);
        assert_eq!(apply.outcome.skipped, 2);
        assert_eq!(apply.next_chunk().unwrap().commands[0].line, 4);
    }

    #[test]
    fn import_decoding_refuses_invalid_utf8_with_its_line() {
        assert_eq!(
            decode_import(b"SET a 1\nSET b 2").unwrap(),
            "SET a 1\nSET b 2"
        );
        assert_eq!(
            decode_import(b"\xEF\xBB\xBFPING").unwrap(),
            "PING",
            "BOM dropped"
        );
        let error = decode_import(b"SET a 1\nSET b 2\nSET c \xff\xfe\n").unwrap_err();
        assert!(error.starts_with("line 3 "), "{error}");
        // The escaped form of the same bytes is what the file should hold.
        let escaped = parse_script(r#"SET c "\xff\xfe""#).unwrap();
        assert_eq!(escaped[0].args[2], vec![0xff, 0xfe]);
    }

    #[test]
    fn import_review_flags_destructive_and_admin_lines() {
        let script = "SET a 1\nGET a\nFLUSHALL\nCONFIG SET maxmemory 1\nCONFIG GET maxmemory\n\
                      SCRIPT LOAD \"return 1\"\nDEL a\nSHUTDOWN NOSAVE\nACL SETUSER bob on\nDEBUG SLEEP 0\n\
                      MODULE LOAD /x.so\nFLUSHDB\nHSET h f v";
        let review = ImportReview::of(&parse_script(script).unwrap());
        assert_eq!(review.total, 13);
        let flagged: Vec<usize> = review.flagged.iter().map(|f| f.line).collect();
        // FLUSHALL, CONFIG SET, SCRIPT LOAD, DEL, SHUTDOWN, ACL SETUSER,
        // DEBUG, MODULE LOAD, FLUSHDB — not SET/GET/CONFIG GET/HSET.
        assert_eq!(flagged, vec![3, 4, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(review.flagged[0].command, "FLUSHALL");
        assert_eq!(review.writes, 11);
        let plain = ImportReview::of(&parse_script("SET a 1\nRPUSH l x").unwrap());
        assert!(plain.flagged.is_empty());
        assert_eq!(plain.writes, 2);
    }

    #[test]
    fn write_block_covers_read_only_profiles_and_locked_production() {
        assert!(write_block_reason(true, ProductionDecision::NotProduction).is_some());
        assert!(
            write_block_reason(false, ProductionDecision::ReadOnly)
                .unwrap()
                .contains("Production is read-only")
        );
        assert!(write_block_reason(false, ProductionDecision::WritesUnlocked).is_none());
        assert!(write_block_reason(false, ProductionDecision::NotProduction).is_none());
    }

    #[test]
    fn selected_database_tracks_the_last_select() {
        let commands = parse_script("GET a\nSELECT 3\nSET b c\nSELECT 7\nGET d").unwrap();
        assert_eq!(selected_database(&commands), Some(7));
        assert_eq!(selected_database(&commands[..3]), Some(3));
        assert_eq!(selected_database(&commands[..1]), None);
        // A SELECT that does not parse as a number is no switch at all.
        let odd = parse_script("SELECT later\nGET a").unwrap();
        assert_eq!(selected_database(&odd), None);
    }

    #[test]
    fn chunked_apply_runs_every_line_once_and_collects_failures() {
        // A scripted fake: chunk 1 runs clean, chunk 2 aborts at its second
        // line, the resumed chunk aborts at its first, the rest run.
        let file = (1..=420)
            .map(|i| format!("SET k{i} v{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let commands = parse_script(&file).unwrap();
        let mut apply = LineApply::new(commands);
        assert_eq!(apply.total(), 420);

        let chunk = apply.next_chunk().unwrap();
        assert_eq!(chunk.commands.len(), LINE_CHUNK);
        assert!(!chunk.prepended_select);
        apply.apply(&chunk, ChunkResult::Ok);
        assert_eq!(apply.processed(), 200);

        let chunk = apply.next_chunk().unwrap();
        apply.apply(
            &chunk,
            ChunkResult::Failed("line 2: ERR scripted".to_string()),
        );
        assert_eq!(apply.outcome.ok, 201);
        assert_eq!(apply.outcome.failed.len(), 1);
        assert_eq!(apply.outcome.failed[0].line, 202);
        assert_eq!(apply.outcome.failed[0].command, "SET k202 v202");
        assert_eq!(apply.outcome.failed[0].error, "ERR scripted");

        // The resume chunk starts right after the failed line.
        let chunk = apply.next_chunk().unwrap();
        assert_eq!(chunk.commands[0].line, 203);
        apply.apply(&chunk, ChunkResult::Failed("line 1: ERR again".to_string()));
        assert_eq!(apply.outcome.failed[1].line, 203);
        let chunk = apply.next_chunk().unwrap();
        assert_eq!(chunk.commands[0].line, 204);
        assert_eq!(chunk.commands.len(), LINE_CHUNK, "chunks stay capped");
        apply.apply(&chunk, ChunkResult::Ok);
        let chunk = apply.next_chunk().unwrap();
        assert_eq!(chunk.commands[0].line, 404);
        assert_eq!(chunk.commands.len(), 420 - 403);
        apply.apply(&chunk, ChunkResult::Ok);
        assert!(apply.is_done());
        assert_eq!(apply.outcome.ok, 418);
        assert_eq!(apply.summary("commands"), "418 commands ok, 2 failed");
    }

    #[test]
    fn resume_chunks_reselect_the_database() {
        let file = (1..=210)
            .map(|i| {
                if i == 150 {
                    "SELECT 3".to_string()
                } else {
                    format!("SET k{i} v")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let commands = parse_script(&file).unwrap();
        let mut apply = LineApply::new(commands);
        let chunk = apply.next_chunk().unwrap();
        assert!(!chunk.prepended_select, "nothing consumed yet");
        apply.apply(&chunk, ChunkResult::Ok);
        // The SELECT at file line 150 ran in the first chunk, so every
        // following chunk re-establishes db 3 before its own lines.
        let chunk = apply.next_chunk().unwrap();
        assert_eq!(chunk.commands.len(), 10);
        assert!(chunk.prepended_select);
        assert!(chunk.script.starts_with("SELECT 3\nSET k201 v"));
        // The driver's "line N" counts the prepended SELECT: its line 2 is
        // the chunk's first user command (file line 201).
        apply.apply(
            &chunk,
            ChunkResult::Failed("line 2: ERR shifted".to_string()),
        );
        assert_eq!(apply.outcome.failed[0].line, 201);
        assert_eq!(apply.outcome.failed[0].error, "ERR shifted");
        // The resume starts at file line 202 and still re-selects db 3.
        let chunk = apply.next_chunk().unwrap();
        assert!(chunk.prepended_select);
        assert!(chunk.script.starts_with("SELECT 3\nSET k202 v"));
        assert_eq!(chunk.commands[0].line, 202);
    }

    fn page_rows(names: &[&str], complete: bool, cursor: &str) -> Vec<Vec<CellValue>> {
        names
            .iter()
            .map(|name| {
                vec![
                    CellValue::String((*name).to_string()),
                    CellValue::String("string".into()),
                    CellValue::Null,
                    CellValue::Bool(complete),
                    CellValue::String(cursor.to_string()),
                ]
            })
            .collect()
    }

    #[test]
    fn dry_run_pages_the_scan_and_continues_from_the_cursor() {
        let mut run = DryRun::new("session:*", 12_000);
        assert_eq!(
            run.next_command().as_deref(),
            Some("SQLLY.KEYS session:* 5000")
        );
        run.absorb(&page_rows(&["session:1", "session:2"], false, "77"));
        assert_eq!(run.keys.len(), 2);
        assert!(run.is_partial());
        assert_eq!(
            run.next_command().as_deref(),
            Some("SQLLY.KEYS session:* 5000 77"),
            "the next page continues from the reported cursor"
        );
        run.absorb(&page_rows(&["session:3"], true, "0"));
        assert!(run.complete && !run.is_partial());
        assert_eq!(run.next_command(), None);
        assert_eq!(run.status(), "3 keys match");
        let mut empty = DryRun::new("none:*", 10);
        empty.absorb(&page_rows(&[], true, "0"));
        assert_eq!(empty.status(), "0 keys match — nothing to do");
    }

    #[test]
    fn dry_run_caps_at_its_limit_and_never_outruns_it() {
        let mut run = DryRun::new("*", 3);
        assert_eq!(run.next_command().as_deref(), Some("SQLLY.KEYS * 3"));
        run.absorb(&page_rows(&["a", "b", "c", "d"], false, "5"));
        assert_eq!(run.keys.len(), 3, "cut at the limit the dialog promised");
        assert!(run.capped());
        assert_eq!(run.next_command(), None);
        assert!(run.status().contains("partial: the scan capped at 3 keys"));
        // A driver without the cursor column leaves an incomplete walk
        // nowhere to continue: capped, never silently complete.
        let mut old = DryRun::new("*", 100);
        let mut rows = page_rows(&["a"], false, "9");
        rows[0].truncate(4);
        old.absorb(&rows);
        assert!(old.capped() && old.next_command().is_none());
    }

    #[test]
    fn pacer_halves_slow_batches_down_to_the_floor_and_doubles_fast_ones() {
        let mut pacer = BatchPacer::new(UNLINK_BATCH);
        assert_eq!(pacer.size(), UNLINK_BATCH);
        pacer.record(std::time::Duration::from_millis(1500));
        assert_eq!(pacer.size(), 250);
        pacer.record(std::time::Duration::from_secs(5));
        assert_eq!(pacer.size(), 125);
        for _ in 0..10 {
            pacer.record(std::time::Duration::from_secs(5));
        }
        assert_eq!(pacer.size(), PACER_FLOOR, "never below the floor");
        pacer.record(std::time::Duration::from_millis(100));
        assert_eq!(pacer.size(), PACER_FLOOR * 2);
        for _ in 0..10 {
            pacer.record(std::time::Duration::from_millis(10));
        }
        assert_eq!(pacer.size(), UNLINK_BATCH, "never above the ceiling");
        // Middling batches hold their size.
        pacer.record(std::time::Duration::from_millis(600));
        assert_eq!(pacer.size(), UNLINK_BATCH);
        // The line apply paces its chunks the same way: a slow chunk halves
        // the next one.
        let lines: Vec<String> = (0..LINE_CHUNK * 2).map(|i| format!("SET k{i} v")).collect();
        let mut apply = LineApply::new(parse_script(&lines.join("\n")).unwrap());
        assert_eq!(apply.next_chunk().unwrap().commands.len(), LINE_CHUNK);
        apply.record_chunk(std::time::Duration::from_secs(2));
        assert_eq!(apply.next_chunk().unwrap().commands.len(), LINE_CHUNK / 2);
    }

    #[test]
    fn fatal_results_stop_with_every_processed_line_accounted() {
        let commands = parse_script("SET a 1\nSET b 2\nSET c 3").unwrap();
        let mut apply = LineApply::new(commands);
        let chunk = apply.next_chunk().unwrap();
        apply.apply(&chunk, ChunkResult::Fatal("connection lost".to_string()));
        assert!(apply.is_done());
        assert_eq!(apply.outcome.ok, 0);
        assert_eq!(apply.outcome.failed.len(), 1);
        assert!(apply.summary("commands").contains("stopped early"));
    }
}
