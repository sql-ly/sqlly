//! Command IntelliSense for Redis and its protocol relatives: completion,
//! hover and signature help over a vocabulary parsed from `COMMAND DOCS`
//! and `COMMAND INFO`.
//!
//! Redis is not a SQL dialect, so none of the SQL completion layers apply.
//! This is a separate provider over the same `CompletionItem` / `HoverInfo`
//! / `SignatureHelp` types the editor already renders:
//!
//! - [`CommandVocabulary`]: the commands, from the [`bundled`] snapshot
//!   (`commands.json`, generated from Redis 7.4 and Redis Stack) or from the connected server's own
//!   docs via the driver's `SQLLY.COMMANDS` rows, so a server-specific or
//!   module command completes too.
//! - [`RedisKeyIndex`]: key names from the explorer's last scan, per
//!   logical database, for the key slots of a command.
//! - [`complete`], [`hover_at`], [`signature_help_at`]: each looks at the
//!   caret's line only, so a 5,000-line script completes as fast as one line.
//!
//! The safety classification in `query::safety::redis` and the vocabulary
//! must agree; `vocabulary::tests` holds the one assertion that keeps them
//! in step.

mod complete;
mod context;
mod hover;
mod keys;
pub mod spec;
mod vocabulary;

pub use complete::{CompletionOptions, MAX_KEY_ITEMS, RedisCompletion, complete};
pub use context::{LineContext, SlotExpectation, Word, expectation, line_context, split_words};
pub use hover::{CommandDoc, hover_at, signature_help_at};
pub use keys::{KEY_INDEX_CAP, KnownKey, RedisKeyIndex};
pub use spec::{Arg, ArgType, CommandInfo, CommandSpec};
pub use vocabulary::{CommandBadge, CommandVocabulary, VocabularySource, badges_for, bundled};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intellisense::{AcceptHistory, CompletionKind, HoverInfo};

    fn labels(items: &[crate::intellisense::CompletionItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    /// `text` with `|` marking the caret.
    fn at(text: &str) -> (String, usize) {
        let cursor = text.find('|').expect("caret marker");
        (text.replacen('|', "", 1), cursor)
    }

    fn complete_at(text: &str, options: CompletionOptions<'_>) -> RedisCompletion {
        let (text, cursor) = at(text);
        complete(&text, cursor, bundled(), options)
    }

    fn seeded_keys() -> RedisKeyIndex {
        let mut keys = RedisKeyIndex::default();
        keys.replace(
            "0",
            [
                ("user:1", "hash"),
                ("user:2", "hash"),
                ("users:all", "set"),
                ("order:1", "string"),
                ("has space", "string"),
            ]
            .into_iter()
            .map(|(name, kind)| KnownKey {
                name: name.into(),
                kind: kind.into(),
            }),
        );
        keys.replace(
            "3",
            [KnownKey {
                name: "session:a".into(),
                kind: "string".into(),
            }],
        );
        keys
    }

    #[test]
    fn command_names_complete_at_line_start() {
        let out = complete_at("HSE|", CompletionOptions::default());
        assert_eq!(labels(&out.items), vec!["HSET", "HSETNX"]);
        assert_eq!(out.replace_start, 0);
        assert_eq!(out.items[0].kind, CompletionKind::Function);
        assert!(!out.items[0].detail.is_empty());
        let out = complete_at("PING\n  js|\nGET k", CompletionOptions::default());
        assert!(labels(&out.items).iter().all(|l| l.starts_with("JSON.")));
        assert_eq!(out.replace_start, 7);
        // No prefix: everything, data structures before administration.
        let out = complete_at("|", CompletionOptions::default());
        assert!(out.items.len() > 400);
        let get = out.items.iter().position(|i| i.label == "GET").unwrap();
        let config = out.items.iter().position(|i| i.label == "CONFIG").unwrap();
        let sqlly = out
            .items
            .iter()
            .position(|i| i.label == "SQLLY.KEYS")
            .unwrap();
        assert!(get < config && config < sqlly);
    }

    #[test]
    fn recently_accepted_commands_rank_first() {
        let mut history = AcceptHistory::default();
        history.record("HSETNX");
        let out = complete_at(
            "HSE|",
            CompletionOptions {
                history: Some(&history),
                ..CompletionOptions::default()
            },
        );
        assert_eq!(labels(&out.items), vec!["HSETNX", "HSET"]);
    }

    #[test]
    fn option_tokens_complete_by_position() {
        let out = complete_at("ZRANGE k 0 -1 |", CompletionOptions::default());
        assert_eq!(
            labels(&out.items),
            vec!["BYLEX", "BYSCORE", "LIMIT", "REV", "WITHSCORES"]
        );
        assert!(out.items.iter().all(|i| i.kind == CompletionKind::Keyword));
        let out = complete_at("ZRANGE k 0 -1 WITH|", CompletionOptions::default());
        assert_eq!(labels(&out.items), vec!["WITHSCORES"]);
        assert_eq!(out.items[0].replace_span, Some((14, 18)));
        let out = complete_at("SET k v EX 10 |", CompletionOptions::default());
        assert_eq!(labels(&out.items), vec!["GET", "NX", "XX"]);
        let out = complete_at("ZRANGE k 0 -1 LIMIT |", CompletionOptions::default());
        assert!(out.items.is_empty());
        assert!(out.reason.contains("offset"), "{}", out.reason);
        // Nothing fits after a complete GET.
        let out = complete_at("GET k |", CompletionOptions::default());
        assert!(out.items.is_empty(), "{:?}", labels(&out.items));
    }

    #[test]
    fn subcommands_complete_after_a_container() {
        let out = complete_at("XINFO |", CompletionOptions::default());
        assert_eq!(
            labels(&out.items),
            vec!["CONSUMERS", "GROUPS", "HELP", "STREAM"]
        );
        let out = complete_at("config g|", CompletionOptions::default());
        assert_eq!(labels(&out.items), vec!["GET"]);
        // After the subcommand, its own grammar applies.
        let out = complete_at("CLIENT LIST |", CompletionOptions::default());
        assert_eq!(labels(&out.items), vec!["ID", "TYPE"]);
    }

    #[test]
    fn key_names_come_from_the_seeded_scan_for_the_tabs_database() {
        let keys = seeded_keys();
        let options = CompletionOptions {
            database: "0",
            keys: &keys,
            history: None,
        };
        let out = complete_at("GET us|", options);
        assert_eq!(labels(&out.items), vec!["user:1", "user:2", "users:all"]);
        assert_eq!(out.items[0].kind, CompletionKind::Table);
        assert_eq!(out.items[0].detail, "hash key");
        assert_eq!(out.items[0].insert_text, "user:1");
        // Keys with spaces insert quoted; a quoted prefix is matched unquoted.
        let out = complete_at("GET has|", options);
        assert_eq!(out.items[0].insert_text, "\"has space\"");
        let out = complete_at("GET \"us|", options);
        assert_eq!(labels(&out.items).len(), 3);
        assert_eq!(out.replace_start, 4);
        // Repeating and later key slots too.
        let out = complete_at("MGET order:1 us|", options);
        assert_eq!(labels(&out.items).len(), 3);
        let out = complete_at("HSET us|", options);
        assert_eq!(labels(&out.items).len(), 3);
        // Not in a value slot.
        let out = complete_at("SET user:1 us|", options);
        assert!(out.items.is_empty());
        // Another database sees its own keys.
        let out = complete_at(
            "GET s|",
            CompletionOptions {
                database: "3",
                ..options
            },
        );
        assert_eq!(labels(&out.items), vec!["session:a"]);
    }

    #[test]
    fn comments_json_lines_and_unknown_commands_offer_nothing() {
        assert!(
            complete_at("# GET |", CompletionOptions::default())
                .items
                .is_empty()
        );
        assert!(
            complete_at("-- GET |", CompletionOptions::default())
                .items
                .is_empty()
        );
        assert!(
            complete_at("[\"GET\", \"|", CompletionOptions::default())
                .items
                .is_empty()
        );
        let out = complete_at("NOSUCH k |", CompletionOptions::default());
        assert!(out.items.is_empty());
        assert!(out.reason.contains("NOSUCH"));
    }

    #[test]
    fn hover_shows_the_command_with_safety_badges() {
        let (text, offset) = at("SET k v\nFLU|SHALL ASYNC\nGET k");
        let (info, span) = hover_at(&text, offset, bundled()).expect("hover");
        let HoverInfo::Command(doc) = info else {
            panic!("{info:?}");
        };
        assert_eq!(doc.name, "FLUSHALL");
        assert_eq!(doc.badges, vec![CommandBadge::Destructive]);
        assert_eq!(doc.badge_text(), "Destructive");
        assert_eq!(span, (8, 16));
        assert!(doc.syntax.starts_with("FLUSHALL"));
        let (text, offset) = at("HS|ET h f v");
        let (HoverInfo::Command(doc), _) = hover_at(&text, offset, bundled()).unwrap() else {
            panic!()
        };
        assert_eq!(doc.badges, vec![CommandBadge::Write]);
        let (text, offset) = at("GE|T k");
        let (HoverInfo::Command(doc), _) = hover_at(&text, offset, bundled()).unwrap() else {
            panic!()
        };
        assert!(doc.badges.is_empty());
        // Blocking looks at the arguments: BLPOP with timeout 0 never answers.
        let (text, offset) = at("BL|POP q 0");
        let (HoverInfo::Command(doc), _) = hover_at(&text, offset, bundled()).unwrap() else {
            panic!()
        };
        assert!(
            doc.badges
                .iter()
                .any(|b| matches!(b, CommandBadge::Blocking(_))),
            "{:?}",
            doc.badges
        );
        // A subcommand hovers as one unit.
        let (text, offset) = at("CONFIG G|ET maxmemory");
        let (HoverInfo::Command(doc), span) = hover_at(&text, offset, bundled()).unwrap() else {
            panic!()
        };
        assert_eq!(doc.name, "CONFIG GET");
        assert_eq!(span, (0, 10));
        // An option token explains itself.
        let (text, offset) = at("ZRANGE k 0 -1 WITH|SCORES");
        let (HoverInfo::Constant { doc, slot }, _) = hover_at(&text, offset, bundled()).unwrap()
        else {
            panic!()
        };
        assert_eq!(doc.name, "WITHSCORES");
        assert_eq!(slot.as_deref(), Some("ZRANGE"));
        // A value is not documented.
        let (text, offset) = at("GET k|");
        assert!(hover_at(&text, offset, bundled()).is_none());
    }

    #[test]
    fn signature_help_tracks_the_active_argument() {
        let (text, cursor) = at("ZRANGE k |");
        let help = signature_help_at(&text, cursor, bundled()).expect("help");
        assert_eq!(
            help.label,
            "ZRANGE key start stop [BYSCORE | BYLEX] [REV] [LIMIT offset count] [WITHSCORES]"
        );
        assert_eq!(help.active_parameter, Some(1));
        assert_eq!(help.parameters[1], "start");
        assert_eq!(
            &help.label[help.parameter_ranges[1].0..help.parameter_ranges[1].1],
            "start"
        );
        let (text, cursor) = at("ZRANGE k 0 -1 |");
        let help = signature_help_at(&text, cursor, bundled()).expect("help");
        assert_eq!(help.active_parameter, Some(3));
        assert_eq!(help.active_values, vec!["BYSCORE", "BYLEX"]);
        let (text, cursor) = at("DEL a |");
        let help = signature_help_at(&text, cursor, bundled()).expect("help");
        assert_eq!(
            help.summary.as_deref().unwrap_or("").split(" · ").next(),
            Some("Destructive")
        );
        assert_eq!(help.active_parameter, Some(0));
        let (text, cursor) = at("ZRAN|");
        assert!(signature_help_at(&text, cursor, bundled()).is_none());
        let (text, cursor) = at("CONFIG |");
        assert!(signature_help_at(&text, cursor, bundled()).is_none());
    }

    /// `SQLLY.ON <target> <command …>`: the target slot offers the keyword
    /// targets, and past it the wrapped command completes, hovers and signs
    /// as if it started the line, while the prefix's card carries the
    /// wrapped command's safety badges.
    #[test]
    fn node_targeting_prefix_completes_targets_then_the_wrapped_command() {
        let out = complete_at("SQLLY.ON |", CompletionOptions::default());
        assert_eq!(labels(&out.items), vec!["all", "masters", "replicas"]);
        let out = complete_at("sqlly.on rep|", CompletionOptions::default());
        assert_eq!(labels(&out.items), vec!["replicas"]);
        assert_eq!(out.items[0].replace_span, Some((9, 12)));
        let out = complete_at("SQLLY.ON masters FLU|", CompletionOptions::default());
        assert_eq!(labels(&out.items), vec!["FLUSHALL", "FLUSHDB"]);
        assert_eq!(out.replace_start, 17);
        let out = complete_at(
            "SQLLY.ON 10.0.0.1:7000 CONFIG |",
            CompletionOptions::default(),
        );
        assert!(
            labels(&out.items).contains(&"GET"),
            "{:?}",
            labels(&out.items)
        );
        let keys = seeded_keys();
        let out = complete_at(
            "SQLLY.ON replicas HGETALL us|",
            CompletionOptions {
                database: "0",
                keys: &keys,
                history: None,
            },
        );
        assert!(labels(&out.items).contains(&"user:1"));

        let (text, offset) = at("SQLLY.ON masters FLUSH|ALL");
        let (HoverInfo::Command(doc), span) = hover_at(&text, offset, bundled()).unwrap() else {
            panic!()
        };
        assert_eq!(doc.name, "FLUSHALL");
        assert_eq!(span, (17, 25));
        assert_eq!(doc.badges, vec![CommandBadge::Destructive]);
        let (text, offset) = at("SQL|LY.ON masters FLUSHALL");
        let (HoverInfo::Command(doc), _) = hover_at(&text, offset, bundled()).unwrap() else {
            panic!()
        };
        assert_eq!(doc.name, "SQLLY.ON");
        assert_eq!(
            doc.syntax,
            "SQLLY.ON <masters | replicas | all | host:port[,host:port]> command [arg [arg ...]]"
        );
        assert_eq!(doc.badges, vec![CommandBadge::Destructive]);
        let (text, offset) = at("SQLLY.ON mas|ters DBSIZE");
        let (HoverInfo::Constant { doc, slot }, _) = hover_at(&text, offset, bundled()).unwrap()
        else {
            panic!()
        };
        assert_eq!(doc.name, "masters");
        assert_eq!(slot.as_deref(), Some("SQLLY.ON"));

        let (text, cursor) = at("SQLLY.ON |");
        let help = signature_help_at(&text, cursor, bundled()).expect("help");
        assert!(help.label.starts_with("SQLLY.ON "), "{}", help.label);
        assert_eq!(help.active_parameter, Some(0));
        let (text, cursor) = at("SQLLY.ON all ZRANGE k |");
        let help = signature_help_at(&text, cursor, bundled()).expect("help");
        assert!(help.label.starts_with("ZRANGE key start"), "{}", help.label);
        assert_eq!(help.active_parameter, Some(1));
    }

    /// AGENTS.md: the tokenizer and IntelliSense must stay performant. A
    /// completion pass looks at one line, so a 5,000-line script costs
    /// the same as one line.
    #[test]
    fn completion_on_a_five_thousand_line_script_stays_under_five_milliseconds() {
        let vocab = bundled();
        let keys = seeded_keys();
        let options = CompletionOptions {
            database: "0",
            keys: &keys,
            history: None,
        };
        let mut script = String::new();
        for i in 0..5_000 {
            script.push_str(&format!("HSET user:{i} name \"user {i}\" age {}\n", i % 90));
        }
        let end = script.len();
        script.push_str("ZRANGE k 0 -1 ");
        // Warm the bundled vocabulary and the page cache before timing.
        let _ = complete(&script, script.len(), vocab, options);
        let started = std::time::Instant::now();
        let mid = end / 2;
        let out_mid = complete(&script, mid, vocab, options);
        let out_end = complete(&script, script.len(), vocab, options);
        let _ = hover_at(&script, mid, vocab);
        let _ = signature_help_at(&script, script.len(), vocab);
        let elapsed = started.elapsed();
        assert!(!out_end.items.is_empty());
        let _ = out_mid;
        assert!(
            elapsed < std::time::Duration::from_millis(5),
            "completion on a 5,000-line script took {elapsed:?}"
        );
    }
}
