//! Completion items for the caret's slot: command names at the start of a
//! line, subcommands after a container, option tokens by position, and key
//! names from the explorer's last scan in key slots.

use super::context::{expectation, line_context};
use super::keys::RedisKeyIndex;
use super::spec::CommandSpec;
use super::vocabulary::CommandVocabulary;
use crate::intellisense::{AcceptHistory, CompletionItem, CompletionKind, CompletionSource};
use crate::query::safety::redis::quote_argument;

/// How many key names one popup offers at most.
pub const MAX_KEY_ITEMS: usize = 100;

/// What completion needs besides the text: which logical database the tab
/// is on (for key names), the scanned keys, and what the user accepted
/// before (recently used commands rank first).
#[derive(Debug, Clone, Copy)]
pub struct CompletionOptions<'a> {
    pub database: &'a str,
    pub keys: &'a RedisKeyIndex,
    pub history: Option<&'a AcceptHistory>,
}

impl Default for CompletionOptions<'static> {
    fn default() -> Self {
        static EMPTY: RedisKeyIndex = RedisKeyIndex::new();
        Self {
            database: "0",
            keys: &EMPTY,
            history: None,
        }
    }
}

/// The popup's items plus where the accepted item replaces from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedisCompletion {
    pub items: Vec<CompletionItem>,
    pub replace_start: usize,
    /// Why the list is what it is (for the feedback report).
    pub reason: String,
}

const COMMAND_SCORE: i32 = 100;
const SUBCOMMAND_SCORE: i32 = 100;
const TOKEN_SCORE: i32 = 100;
const KEY_SCORE: i32 = 90;

/// Data-structure groups first, in the order a user reaches for them;
/// administration and modules after; SQLly's own virtual commands last.
fn group_rank(group: &str) -> i32 {
    match group {
        "generic" => 0,
        "string" => 1,
        "hash" => 2,
        "list" => 3,
        "set" => 4,
        "sorted-set" => 5,
        "stream" => 6,
        "bitmap" => 7,
        "hyperloglog" => 8,
        "geo" => 9,
        "pubsub" => 10,
        "transactions" => 11,
        "scripting" => 12,
        "connection" => 13,
        "server" => 14,
        "cluster" => 15,
        "sqlly" => 40,
        _ => 20,
    }
}

fn starts_with_ci(label: &str, prefix: &str) -> bool {
    label.len() >= prefix.len()
        && label
            .as_bytes()
            .iter()
            .zip(prefix.as_bytes())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

fn item(
    label: &str,
    insert_text: String,
    kind: CompletionKind,
    score: i32,
    detail: String,
    span: (usize, usize),
) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        insert_text,
        kind,
        source: CompletionSource::Builtin,
        score,
        detail,
        replace_span: Some(span),
    }
}

fn command_item(
    spec: &CommandSpec,
    label: &str,
    prefix: &str,
    history: Option<&AcceptHistory>,
    span: (usize, usize),
    base: i32,
) -> CompletionItem {
    let mut score = base - group_rank(&spec.group);
    if label.eq_ignore_ascii_case(prefix) {
        score += crate::intellisense::EXACT_LABEL_BOOST;
    }
    if let Some(history) = history {
        score += history.boost(label);
    }
    if spec.deprecated_since.is_some() {
        score -= 50;
    }
    let mut detail = spec.summary.clone();
    if spec.is_container() {
        detail = format!("{} ({} subcommands)", detail, spec.subcommands.len());
    }
    if spec.deprecated_since.is_some() {
        detail = format!("deprecated · {detail}");
    }
    item(
        label,
        label.to_string(),
        CompletionKind::Function,
        score,
        detail,
        span,
    )
}

/// Complete at `cursor`. Looks only at the caret's line.
pub fn complete(
    text: &str,
    cursor: usize,
    vocabulary: &CommandVocabulary,
    options: CompletionOptions<'_>,
) -> RedisCompletion {
    let mut ctx = line_context(text, cursor);
    // After `SQLLY.ON <target>` the wrapped command completes as if it
    // started the line.
    ctx.skip_on_node_prefix();
    let mut out = RedisCompletion {
        replace_start: ctx.current.start,
        ..Default::default()
    };
    if ctx.is_opaque() {
        out.reason = "comment or JSON command line".into();
        return out;
    }
    let prefix = ctx.current.text.as_str();
    let span = (ctx.current.start, ctx.current.end);
    if ctx.before.is_empty() {
        for (name, spec) in &vocabulary.commands {
            if starts_with_ci(name, prefix) {
                out.items.push(command_item(
                    spec,
                    name,
                    prefix,
                    options.history,
                    span,
                    COMMAND_SCORE,
                ));
            }
        }
        out.reason = "command names at the start of a line".into();
        finish(&mut out);
        return out;
    }
    let before = ctx.before_texts();
    let Some((spec, consumed)) = vocabulary.resolve(&before) else {
        out.reason = format!("unknown command {}", before[0].to_ascii_uppercase());
        return out;
    };
    if spec.is_container() && before.len() == 1 {
        for (sub, sub_spec) in &spec.subcommands {
            if starts_with_ci(sub, prefix) {
                out.items.push(command_item(
                    sub_spec,
                    sub,
                    prefix,
                    options.history,
                    span,
                    SUBCOMMAND_SCORE,
                ));
            }
        }
        out.reason = format!("subcommands of {}", spec.name);
        finish(&mut out);
        return out;
    }
    let typed = &before[consumed..];
    let slot = expectation(spec, typed);
    for (token, arg) in &slot.tokens {
        if starts_with_ci(token, prefix) {
            let mut score = TOKEN_SCORE;
            if token.eq_ignore_ascii_case(prefix) {
                score += crate::intellisense::EXACT_LABEL_BOOST;
            }
            let detail = if arg.arguments.is_empty() && arg.token.as_deref() == Some(token) {
                format!("{} option of {}", arg.name, spec.name)
            } else {
                arg.syntax()
            };
            out.items.push(item(
                token,
                token.to_string(),
                CompletionKind::Keyword,
                score,
                detail,
                span,
            ));
        }
    }
    if slot.wants_key() {
        for key in options
            .keys
            .matching(options.database, prefix, MAX_KEY_ITEMS)
        {
            let mut score = KEY_SCORE;
            if key.name == prefix {
                score += crate::intellisense::EXACT_LABEL_BOOST;
            }
            out.items.push(item(
                &key.name,
                quote_argument(key.name.as_bytes()),
                CompletionKind::Table,
                score,
                format!("{} key", key.kind),
                span,
            ));
        }
    }
    out.reason = match (slot.positional, slot.tokens.is_empty()) {
        (Some(arg), _) => format!(
            "{} slot of {}: {}",
            arg.display,
            spec.name,
            arg.arg_type.label()
        ),
        (None, false) => format!("options of {}", spec.name),
        (None, true) => format!("{} takes nothing more here", spec.name),
    };
    finish(&mut out);
    out
}

fn finish(out: &mut RedisCompletion) {
    out.items
        .sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
}
