//! Where the caret sits on a command line, and which documented argument
//! that position belongs to.
//!
//! Everything here looks at one line only — a Redis script is one command
//! per line, so completion on a 5,000-line script costs the same as on a
//! one-line one.

use super::spec::{Arg, ArgType, CommandSpec};

/// One word of the line as typed: its unquoted text and its byte span in
/// the document (including any quotes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub text: String,
    pub start: usize,
    pub end: usize,
    /// The word began with `"` or `'`.
    pub quoted: bool,
}

/// The line under the caret, split into the words before the caret and
/// the (possibly empty) word the caret is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineContext {
    pub line_start: usize,
    pub line_end: usize,
    /// Complete words before the caret's word.
    pub before: Vec<Word>,
    /// The word the caret is in or right after; empty text when the caret
    /// follows whitespace.
    pub current: Word,
    /// Every word on the line, caret or not (for hover).
    pub all: Vec<Word>,
}

impl LineContext {
    /// `before` as plain strings, which the vocabulary and walker take.
    pub fn before_texts(&self) -> Vec<String> {
        self.before.iter().map(|w| w.text.clone()).collect()
    }

    /// `SQLLY.ON <target> <command …>`: once the caret is past the target,
    /// the rest of the line completes and signs as the command it wraps, so
    /// the prefix's words are dropped from `before` and `all`. Returns how
    /// many words were dropped.
    pub fn skip_on_node_prefix(&mut self) -> usize {
        let n = on_node_prefix_len(&self.before);
        if n > 0 {
            self.before.drain(..n);
            self.all.drain(..n.min(self.all.len()));
        }
        n
    }

    /// True for a line completion must leave alone: a comment, or a JSON
    /// command line.
    pub fn is_opaque(&self) -> bool {
        self.all.first().is_some_and(|w| {
            w.text.starts_with('#')
                || w.text.starts_with("--")
                || w.text.starts_with('[')
                || w.text.starts_with('{')
        })
    }
}

/// How many leading words are `SQLLY.ON <target>` prefixes (two per prefix;
/// prefixes may nest).
pub fn on_node_prefix_len(words: &[Word]) -> usize {
    let mut n = 0;
    while words.len() >= n + 2
        && words[n]
            .text
            .eq_ignore_ascii_case(crate::query::safety::redis::ON_NODE_COMMAND)
    {
        n += 2;
    }
    n
}

/// Split the line containing `cursor` into words. Lenient where the strict
/// script tokenizer errors: an unterminated quote simply runs to the end
/// of the line, because the user is still typing it.
pub fn line_context(text: &str, cursor: usize) -> LineContext {
    let cursor = cursor.min(text.len());
    let line_start = text[..cursor].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[cursor..].find('\n').map_or(text.len(), |i| cursor + i);
    let line = &text[line_start..line_end];
    let all = split_words(line, line_start);
    let mut before = Vec::new();
    let mut current = None;
    for word in &all {
        if word.end < cursor || (word.end == cursor && word.quoted && closes_quote(text, word)) {
            before.push(word.clone());
        } else if word.start <= cursor && current.is_none() {
            current = Some(Word {
                text: word_text_up_to(text, word, cursor),
                start: word.start,
                end: cursor,
                quoted: word.quoted,
            });
        }
    }
    let current = current.unwrap_or(Word {
        text: String::new(),
        start: cursor,
        end: cursor,
        quoted: false,
    });
    LineContext {
        line_start,
        line_end,
        before,
        current,
        all,
    }
}

/// True when a quoted word's last byte is its closing quote.
fn closes_quote(text: &str, word: &Word) -> bool {
    let raw = &text[word.start..word.end];
    raw.len() >= 2 && raw.ends_with(raw.chars().next().unwrap_or('"'))
}

/// The unquoted text of `word` truncated at `cursor` (the caret can sit
/// mid-word).
fn word_text_up_to(text: &str, word: &Word, cursor: usize) -> String {
    let raw = &text[word.start..cursor.max(word.start)];
    unquote(raw)
}

fn unquote(raw: &str) -> String {
    let mut chars = raw.chars();
    match chars.next() {
        Some(q @ ('"' | '\'')) => {
            let mut out = String::new();
            let mut escaped = false;
            for c in chars {
                if escaped {
                    out.push(c);
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    break;
                } else {
                    out.push(c);
                }
            }
            out
        }
        _ => raw.to_string(),
    }
}

/// `redis-cli` word splitting without the error cases: whitespace separates
/// bare words, quotes group, a backslash inside quotes escapes the next
/// character, an unterminated quote runs to the end of the line.
pub fn split_words(line: &str, base: usize) -> Vec<Word> {
    let bytes = line.as_bytes();
    let mut words = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        let quoted = matches!(bytes[i], b'"' | b'\'');
        if quoted {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else {
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        }
        // Keep byte spans on char boundaries for multi-byte text.
        while i < bytes.len() && !line.is_char_boundary(i) {
            i += 1;
        }
        let raw = &line[start..i];
        words.push(Word {
            text: unquote(raw),
            start: base + start,
            end: base + i,
            quoted,
        });
    }
    words
}

/// What the caret's slot accepts, from walking the typed arguments through
/// the command's grammar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotExpectation<'a> {
    /// Fixed words that may come next (`WITHSCORES`, `LIMIT`), each with
    /// the argument that owns it.
    pub tokens: Vec<(&'a str, &'a Arg)>,
    /// The positional argument the caret fills, when there is one.
    pub positional: Option<&'a Arg>,
    /// Which top-level argument the caret is in (for signature help).
    pub active_index: Option<usize>,
}

impl SlotExpectation<'_> {
    /// True when the caret fills a key name.
    pub fn wants_key(&self) -> bool {
        self.positional
            .is_some_and(|arg| arg.arg_type == ArgType::Key)
    }
}

/// Every fixed word an argument list can start an element with (for
/// stopping a greedy `multiple` consumption).
fn stop_tokens(args: &[Arg]) -> Vec<&str> {
    args.iter().flat_map(Arg::tokens).collect()
}

fn matches_token(word: &str, tokens: &[&str]) -> bool {
    tokens.iter().any(|t| t.eq_ignore_ascii_case(word))
}

struct Walker<'a, 't> {
    typed: &'t [String],
    pos: usize,
    /// Optional token-bearing arguments skipped while walking; Redis
    /// accepts most of them in any order, so they stay on offer.
    skipped: Vec<&'a Arg>,
    /// How many blocks deep the walk is.
    depth: usize,
    /// The typed words ran out inside a block: its arguments and the index
    /// of the first one the caret fills.
    inner: Option<(&'a [Arg], usize)>,
    /// The typed words ran out right after a value-taking token (`EX |`):
    /// the caret fills that argument's value.
    pending_value: Option<&'a Arg>,
}

impl<'a, 't> Walker<'a, 't> {
    fn done(&self) -> bool {
        self.pos >= self.typed.len()
    }

    fn peek(&self) -> Option<&'t str> {
        self.typed.get(self.pos).map(String::as_str)
    }

    /// Consume one argument starting at `pos`. Returns false when the
    /// argument is not present at this position.
    fn consume(&mut self, arg: &'a Arg, later_stops: &[&'a str]) -> bool {
        let Some(word) = self.peek() else {
            return false;
        };
        match arg.arg_type {
            ArgType::PureToken => {
                if matches_token(word, &arg.tokens()) {
                    self.pos += 1;
                    true
                } else {
                    false
                }
            }
            ArgType::OneOf => {
                let tokens = arg.tokens();
                if !tokens.is_empty() {
                    // Token-led alternatives: the matching child consumes.
                    if let Some(token) = &arg.token {
                        if !token.eq_ignore_ascii_case(word) {
                            return false;
                        }
                        self.pos += 1;
                    }
                    let Some(word) = self.peek() else {
                        if arg.token.is_some() {
                            self.inner = Some((&arg.arguments, 0));
                        }
                        return true;
                    };
                    for child in &arg.arguments {
                        if matches_token(word, &child.tokens()) {
                            return self.consume(child, later_stops);
                        }
                    }
                    arg.token.is_some()
                } else {
                    // Positional alternatives: one word.
                    self.pos += 1;
                    true
                }
            }
            ArgType::Block => {
                if let Some(token) = &arg.token {
                    if !token.eq_ignore_ascii_case(word) {
                        return false;
                    }
                    self.pos += 1;
                }
                let mut stops = later_stops.to_vec();
                stops.extend(stop_tokens(&arg.arguments));
                self.depth += 1;
                self.walk(&arg.arguments, &stops);
                self.depth -= 1;
                true
            }
            _ => {
                if let Some(token) = &arg.token {
                    if !token.eq_ignore_ascii_case(word) {
                        return false;
                    }
                    self.pos += 1;
                    if self.done() {
                        self.pending_value = Some(arg);
                    } else {
                        self.pos += 1;
                    }
                } else {
                    self.pos += 1;
                }
                true
            }
        }
    }

    /// Walk `args` in order against the typed words from `pos`.
    fn walk(&mut self, args: &'a [Arg], later_stops: &[&'a str]) {
        for (index, arg) in args.iter().enumerate() {
            if self.done() {
                if self.depth > 0 && self.inner.is_none() && self.pending_value.is_none() {
                    self.inner = Some((args, index));
                }
                return;
            }
            let mut stops: Vec<&'a str> = later_stops.to_vec();
            stops.extend(stop_tokens(&args[index + 1..]));
            let present = self.consume(arg, &stops);
            if !present {
                if arg.optional || arg.is_token_bearing() {
                    if arg.is_token_bearing() {
                        self.skipped.push(arg);
                    }
                    continue;
                }
                // A required positional is always "present": the word
                // typed there is it, whatever it is.
                self.pos += 1;
            }
            if arg.multiple {
                // Repeat while the next word is not a later argument's token.
                loop {
                    if self.done() {
                        return;
                    }
                    let Some(word) = self.peek() else {
                        return;
                    };
                    if matches_token(word, &stops) {
                        break;
                    }
                    if arg.multiple_token && !matches_token(word, &arg.tokens()) {
                        break;
                    }
                    let before = self.pos;
                    if !self.consume(arg, &stops) || self.pos == before {
                        break;
                    }
                }
            }
        }
    }
}

/// The slot a positional argument really fills: a token-less block's
/// first argument (`HSET`'s `field value` pair starts with `field`).
fn first_slot(arg: &Arg) -> &Arg {
    match arg.arg_type {
        ArgType::Block if arg.token.is_none() => arg.arguments.first().map_or(arg, first_slot),
        _ => arg,
    }
}

/// Fill `expectation` from the arguments at and after the caret: every
/// token up to the first required argument, and the first positional.
fn offer<'a>(expectation: &mut SlotExpectation<'a>, args: &'a [Arg]) {
    for arg in args {
        if arg.is_token_bearing() {
            for token in arg.tokens() {
                expectation.tokens.push((token, arg));
            }
            if !arg.optional {
                break;
            }
        } else {
            if expectation.positional.is_none() {
                expectation.positional = Some(first_slot(arg));
            }
            if !arg.optional {
                break;
            }
        }
    }
}

/// What the caret may fill after `typed` arguments of `spec` (the command
/// name and subcommand already stripped).
pub fn expectation<'a>(spec: &'a CommandSpec, typed: &[String]) -> SlotExpectation<'a> {
    let mut walker = Walker {
        typed,
        pos: 0,
        skipped: Vec::new(),
        depth: 0,
        inner: None,
        pending_value: None,
    };
    let args = &spec.arguments;
    let mut expectation = SlotExpectation::default();
    // Walk top-level arguments one at a time so the active index is known.
    let mut reached: Option<usize> = None;
    for (index, arg) in args.iter().enumerate() {
        if walker.done() {
            reached = Some(index);
            break;
        }
        let stops = stop_tokens(&args[index + 1..]);
        let before = walker.pos;
        walker.walk(std::slice::from_ref(arg), &stops);
        if walker.done() {
            if walker.inner.is_some() || walker.pending_value.is_some() {
                // The caret is inside this argument.
                reached = Some(index);
                break;
            }
            if arg.multiple && walker.pos > before {
                // The caret continues a repeating argument (`DEL a b |`).
                if !arg.is_token_bearing() || arg.multiple_token {
                    reached = Some(index);
                    break;
                }
                reached = Some(index + 1);
                break;
            }
        }
    }
    if !walker.done() {
        // Words the grammar could not place (options out of order, a value
        // where a token was expected): offer every optional token the line
        // has not used yet, and no positional slot.
        let used: Vec<&str> = typed.iter().map(String::as_str).collect();
        for arg in args.iter().filter(|a| a.is_token_bearing()) {
            for token in arg.tokens() {
                if !matches_token(token, &used) {
                    expectation.tokens.push((token, arg));
                }
            }
        }
        return expectation;
    }
    let from = reached.unwrap_or(args.len());
    expectation.active_index = reached.filter(|i| *i < args.len());
    if let Some(arg) = walker.pending_value {
        expectation.positional = Some(arg);
        return expectation;
    }
    if let Some((inner_args, inner_from)) = walker.inner {
        offer(&mut expectation, &inner_args[inner_from..]);
        return expectation;
    }
    for arg in walker.skipped {
        for token in arg.tokens() {
            expectation.tokens.push((token, arg));
        }
    }
    offer(&mut expectation, &args[from..]);
    expectation
        .tokens
        .dedup_by(|a, b| a.0.eq_ignore_ascii_case(b.0));
    expectation
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis_complete::bundled;

    fn words(line: &str) -> Vec<String> {
        split_words(line, 0).into_iter().map(|w| w.text).collect()
    }

    #[test]
    fn splits_words_leniently() {
        assert_eq!(
            words("SET k \"hello world\""),
            vec!["SET", "k", "hello world"]
        );
        assert_eq!(
            words("SET k \"unterminated"),
            vec!["SET", "k", "unterminated"]
        );
        assert_eq!(words("GET 'it\\'s'"), vec!["GET", "it's"]);
        assert_eq!(words("  PING  "), vec!["PING"]);
        assert_eq!(words("GET ключ"), vec!["GET", "ключ"]);
    }

    #[test]
    fn line_context_finds_the_caret_word_on_the_caret_line() {
        let text = "PING\nHSE\nGET k";
        let ctx = line_context(text, 8);
        assert_eq!(ctx.line_start, 5);
        assert!(ctx.before.is_empty());
        assert_eq!(ctx.current.text, "HSE");
        assert_eq!((ctx.current.start, ctx.current.end), (5, 8));
        let ctx = line_context("ZRANGE k 0 -1 ", 14);
        assert_eq!(ctx.before_texts(), vec!["ZRANGE", "k", "0", "-1"]);
        assert_eq!(ctx.current.text, "");
        assert_eq!(ctx.current.start, 14);
        let ctx = line_context("GET \"us", 7);
        assert_eq!(ctx.current.text, "us");
        assert!(ctx.current.quoted);
        assert_eq!(ctx.current.start, 4);
        let ctx = line_context("GET \"user\" ", 11);
        assert_eq!(ctx.before_texts(), vec!["GET", "user"]);
        assert!(line_context("# GET k", 7).is_opaque());
        assert!(line_context("[\"GET\", \"k\"]", 3).is_opaque());
    }

    fn expect(command: &str, typed: &[&str]) -> (Vec<String>, Option<String>) {
        let (spec, _) = bundled()
            .resolve(&[command.to_string()])
            .expect("known command");
        let typed: Vec<String> = typed.iter().map(|s| s.to_string()).collect();
        let e = expectation(spec, &typed);
        (
            e.tokens.iter().map(|(t, _)| t.to_string()).collect(),
            e.positional
                .map(|a| format!("{}:{}", a.display, a.arg_type.label())),
        )
    }

    #[test]
    fn walks_documented_grammars_to_the_caret_slot() {
        assert_eq!(expect("GET", &[]), (vec![], Some("key:key".into())));
        assert_eq!(expect("GET", &["k"]), (vec![], None));
        let (tokens, positional) = expect("ZRANGE", &["k", "0", "-1"]);
        assert_eq!(
            tokens,
            vec!["BYSCORE", "BYLEX", "REV", "LIMIT", "WITHSCORES"]
        );
        assert_eq!(positional, None);
        let (tokens, _) = expect("ZRANGE", &["k", "0", "-1", "REV"]);
        assert_eq!(tokens, vec!["BYSCORE", "BYLEX", "LIMIT", "WITHSCORES"]);
        let (tokens, positional) = expect("ZRANGE", &["k", "0", "-1", "LIMIT"]);
        assert!(tokens.is_empty(), "{tokens:?}");
        assert_eq!(positional, Some("offset:integer".into()));
        let (tokens, _) = expect("ZRANGE", &["k", "0", "-1", "LIMIT", "0", "10"]);
        assert_eq!(tokens, vec!["BYSCORE", "BYLEX", "REV", "WITHSCORES"]);
        // SET's options may come in any order.
        let (tokens, _) = expect("SET", &["k", "v", "EX", "10"]);
        assert_eq!(tokens, vec!["NX", "XX", "GET"], "KEEPTTL excludes EX");
        assert_eq!(
            expect("SET", &["k", "v", "EX"]).1,
            Some("seconds:integer".into())
        );
        let (tokens, _) = expect("SET", &["k", "v"]);
        assert!(tokens.contains(&"EX".to_string()) && tokens.contains(&"NX".to_string()));
        // Repeating key arguments stay keys.
        assert_eq!(expect("DEL", &["a", "b"]).1, Some("key:key".into()));
        assert_eq!(expect("MGET", &["a"]).1, Some("key:key".into()));
        // A repeating block (HSET field value [field value ...]).
        assert_eq!(expect("HSET", &["h"]).1, Some("field:string".into()));
        assert_eq!(expect("HSET", &["h", "f"]).1, Some("value:string".into()));
        assert_eq!(
            expect("HSET", &["h", "f", "v"]).1,
            Some("field:string".into())
        );
        // XREAD: options then the required STREAMS token.
        let (tokens, _) = expect("XREAD", &[]);
        assert_eq!(tokens, vec!["COUNT", "BLOCK", "STREAMS"]);
        let (_, positional) = expect("XREAD", &["COUNT", "2", "STREAMS"]);
        assert_eq!(positional, Some("key:key".into()));
    }
}
