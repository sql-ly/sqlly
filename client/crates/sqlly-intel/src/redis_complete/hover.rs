//! Hover cards and signature help for command lines: the command's summary,
//! documented syntax and safety badges, the meaning of an option token, and
//! the argument the caret is on.

use super::context::{expectation, line_context, on_node_prefix_len};
use super::spec::{Arg, CommandSpec};
use super::vocabulary::{CommandBadge, CommandVocabulary, badges_for};
use crate::intellisense::{HoverInfo, RoutineKind, SignatureHelp, WordDoc};

/// What a command hover shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDoc {
    /// `ZRANGE`, or `CONFIG GET` for a subcommand.
    pub name: String,
    pub syntax: String,
    pub summary: String,
    pub group: String,
    pub since: Option<String>,
    pub complexity: Option<String>,
    pub deprecated_since: Option<String>,
    pub badges: Vec<CommandBadge>,
}

impl CommandDoc {
    pub fn from_spec(spec: &CommandSpec, line_args: &[String]) -> Self {
        Self {
            name: spec.name.clone(),
            syntax: spec.syntax(),
            summary: spec.summary.clone(),
            group: spec.group.clone(),
            since: spec.since.clone(),
            complexity: spec.complexity.clone(),
            deprecated_since: spec.deprecated_since.clone(),
            badges: badges_for(line_args),
        }
    }

    /// `Destructive · Blocking` for the card's title line, empty when the
    /// command only reads.
    pub fn badge_text(&self) -> String {
        self.badges
            .iter()
            .map(CommandBadge::label)
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// Find the argument (at any depth) that owns `token`.
fn arg_for_token<'a>(args: &'a [Arg], token: &str) -> Option<&'a Arg> {
    for arg in args {
        if arg
            .token
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(token))
        {
            return Some(arg);
        }
        if let Some(found) = arg_for_token(&arg.arguments, token) {
            return Some(found);
        }
    }
    None
}

/// The hover for the word at `offset`, with the byte span it covers.
pub fn hover_at(
    text: &str,
    offset: usize,
    vocabulary: &CommandVocabulary,
) -> Option<(HoverInfo, (usize, usize))> {
    let ctx = line_context(text, offset);
    if ctx.is_opaque() {
        return None;
    }
    let index = ctx
        .all
        .iter()
        .position(|w| w.start <= offset && offset <= w.end)?;
    // `SQLLY.ON <target> <command …>`: a word of the wrapped command hovers
    // as that command; the prefix's own words hover as `SQLLY.ON` (whose
    // card carries the wrapped command's safety badges).
    let prefix = on_node_prefix_len(&ctx.all);
    let (words, index) = if index >= prefix {
        (&ctx.all[prefix..], index - prefix)
    } else {
        let start = index - index % 2;
        (&ctx.all[start..], index - start)
    };
    let args: Vec<String> = words.iter().map(|w| w.text.clone()).collect();
    let (spec, consumed) = vocabulary.resolve(&args)?;
    if index < consumed {
        let span = (words[0].start, words[consumed - 1].end);
        let doc = CommandDoc::from_spec(spec, &args);
        return Some((HoverInfo::Command(Box::new(doc)), span));
    }
    let word = &words[index];
    let arg = arg_for_token(&spec.arguments, &word.text)?;
    let token = arg.token.clone().unwrap_or_else(|| word.text.clone());
    let doc = WordDoc {
        name: token,
        doc: format!(
            "{} option of {}{}",
            arg.name,
            spec.name,
            arg.since
                .as_deref()
                .map(|s| format!(" (since {s})"))
                .unwrap_or_default()
        ),
        syntax: Some(arg.syntax()),
    };
    Some((
        HoverInfo::Constant {
            doc,
            slot: Some(spec.name.clone()),
        },
        (word.start, word.end),
    ))
}

/// Signature help for the command on the caret's line: its documented
/// syntax with the caret's argument marked active. `None` before the
/// command name is complete, or while a container's subcommand is being
/// chosen.
pub fn signature_help_at(
    text: &str,
    cursor: usize,
    vocabulary: &CommandVocabulary,
) -> Option<SignatureHelp> {
    let mut ctx = line_context(text, cursor);
    ctx.skip_on_node_prefix();
    if ctx.is_opaque() || ctx.before.is_empty() {
        return None;
    }
    let before = ctx.before_texts();
    let (spec, consumed) = vocabulary.resolve(&before)?;
    if spec.is_container() && before.len() == 1 {
        return None;
    }
    let typed = &before[consumed..];
    let slot = expectation(spec, typed);
    let mut label = spec.name.clone();
    let mut parameters = Vec::with_capacity(spec.arguments.len());
    let mut parameter_ranges = Vec::with_capacity(spec.arguments.len());
    let mut parameter_docs = Vec::with_capacity(spec.arguments.len());
    for arg in &spec.arguments {
        let syntax = arg.syntax();
        label.push(' ');
        let start = label.len();
        label.push_str(&syntax);
        parameter_ranges.push((start, label.len()));
        parameters.push(syntax);
        parameter_docs.push(arg_doc(arg));
    }
    let mut line_args = before.clone();
    line_args.push(ctx.current.text.clone());
    let badges = badges_for(&line_args);
    let mut summary = spec.summary.clone();
    if !badges.is_empty() {
        let badge_text = badges
            .iter()
            .map(CommandBadge::label)
            .collect::<Vec<_>>()
            .join(" · ");
        summary = if summary.is_empty() {
            badge_text
        } else {
            format!("{badge_text} · {summary}")
        };
    }
    let active_values = slot
        .active_index
        .and_then(|i| spec.arguments.get(i))
        .map(|arg| arg.tokens().into_iter().map(str::to_string).collect())
        .unwrap_or_default();
    Some(SignatureHelp {
        label,
        parameters,
        parameter_ranges,
        parameter_docs,
        active_parameter: slot.active_index,
        kind: RoutineKind::Function,
        open_paren: ctx.line_start,
        summary: (!summary.is_empty()).then_some(summary),
        overload_index: 0,
        overload_count: 1,
        active_values,
    })
}

/// One line about an argument: its type, whether it may repeat or be
/// left out, and the words it accepts.
fn arg_doc(arg: &Arg) -> String {
    let mut parts = vec![arg.arg_type.label().to_string()];
    if arg.optional {
        parts.push("optional".into());
    }
    if arg.multiple {
        parts.push("repeatable".into());
    }
    let tokens = arg.tokens();
    if !tokens.is_empty() && arg.token.is_none() {
        parts.push(format!("one of {}", tokens.join(", ")));
    }
    if let Some(since) = &arg.since {
        parts.push(format!("since {since}"));
    }
    parts.join(", ")
}
