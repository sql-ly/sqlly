//! The set of commands completion knows: the bundled snapshot, or the
//! connected server's own `COMMAND DOCS` / `COMMAND INFO` (through the
//! driver's `SQLLY.COMMANDS` rows), always joined with SQLly's `SQLLY.*`
//! virtual commands so they complete on every server.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::Value;

use super::spec::{CommandSpec, normalize_docs, normalize_info, parse_vocabulary};
use crate::query::safety::redis::{RedisCommand, blocking_reason, command_access};

/// The snapshot generated from the Redis 7.4 and Redis Stack command tables
/// plus a hand-maintained extras list.
const BUNDLED_JSON: &str = include_str!("commands.json");

/// Where a vocabulary came from, for the status text and the feedback report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VocabularySource {
    /// The snapshot compiled into the app.
    Bundled,
    /// The connected server answered `SQLLY.COMMANDS`.
    Server { command_count: usize },
}

/// Hover and completion badges derived from the safety classification, so
/// the popup warns before the run gate does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandBadge {
    /// Deletes data wholesale, reconfigures, or stops the server.
    Destructive,
    /// Writes keys or state.
    Write,
    /// Would never answer on a request/response connection (`SUBSCRIBE`,
    /// `BLPOP … 0`); carries the driver's refusal text.
    Blocking(String),
}

impl CommandBadge {
    /// The short word the hover card prints.
    pub fn label(&self) -> &'static str {
        match self {
            CommandBadge::Destructive => "Destructive",
            CommandBadge::Write => "Write",
            CommandBadge::Blocking(_) => "Blocking",
        }
    }
}

/// Badges for a typed command line (`args[0]` is the name). The destructive
/// and write badges come from [`command_access`]; blocking from
/// [`blocking_reason`], which looks at the arguments (`BLPOP q 0`).
pub fn badges_for(args: &[String]) -> Vec<CommandBadge> {
    if args.is_empty() {
        return Vec::new();
    }
    let command = RedisCommand {
        line: 1,
        args: args.iter().map(|a| a.as_bytes().to_vec()).collect(),
    };
    let mut badges = Vec::new();
    match command_access(&command) {
        crate::query::safety::redis::CommandAccess::Destructive => {
            badges.push(CommandBadge::Destructive)
        }
        crate::query::safety::redis::CommandAccess::Write => badges.push(CommandBadge::Write),
        crate::query::safety::redis::CommandAccess::ReadOnly => {}
    }
    if let Some(reason) = blocking_reason(&command) {
        badges.push(CommandBadge::Blocking(reason));
    }
    badges
}

/// The commands completion offers, keyed by upper-case name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandVocabulary {
    pub commands: BTreeMap<String, CommandSpec>,
    pub source: VocabularySource,
}

impl CommandVocabulary {
    /// Parse a vocabulary from raw `COMMAND DOCS` and `COMMAND INFO` JSON in
    /// either spelling (see `spec::normalize_docs`).
    pub fn from_json(docs: Value, info: Value, source: VocabularySource) -> Self {
        let docs = normalize_docs(docs);
        let info = normalize_info(info);
        Self {
            commands: parse_vocabulary(&docs, &info),
            source,
        }
    }

    /// Build a vocabulary from the driver's `SQLLY.COMMANDS` rows: one
    /// `(name, docs_json, info_json)` per command, each JSON in whichever
    /// spelling the connection produced. Commands the server does not have
    /// are not offered — except SQLly's own `SQLLY.*` virtual commands,
    /// which join from the bundled snapshot. Returns `None` for an empty
    /// row set (the server refused `COMMAND`), so the caller falls back.
    pub fn from_server_rows<'a>(
        rows: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
    ) -> Option<Self> {
        let mut docs = serde_json::Map::new();
        let mut info = serde_json::Map::new();
        for (name, docs_json, info_json) in rows {
            let name = name.to_ascii_uppercase();
            if let Ok(value) = serde_json::from_str::<Value>(docs_json) {
                docs.insert(name.clone(), value);
            }
            if let Ok(value) = serde_json::from_str::<Value>(info_json) {
                info.insert(name, value);
            }
        }
        if docs.is_empty() && info.is_empty() {
            return None;
        }
        let count = docs.len().max(info.len());
        let mut vocabulary = Self::from_json(
            Value::Object(docs),
            Value::Object(info),
            VocabularySource::Server {
                command_count: count,
            },
        );
        for (name, spec) in &bundled().commands {
            if name.starts_with("SQLLY.") {
                vocabulary
                    .commands
                    .entry(name.clone())
                    .or_insert_with(|| spec.clone());
            }
        }
        Some(vocabulary)
    }

    /// The spec for a command name (any case), or `None`.
    pub fn get(&self, name: &str) -> Option<&CommandSpec> {
        self.commands.get(&name.to_ascii_uppercase())
    }

    /// Resolve a typed line's command: the container's subcommand when the
    /// first argument names one, else the command itself. Returns the spec
    /// and how many leading words it consumed (1 or 2).
    pub fn resolve<'a>(&'a self, args: &[String]) -> Option<(&'a CommandSpec, usize)> {
        let spec = self.get(args.first()?)?;
        if spec.is_container()
            && let Some(sub) = args.get(1)
            && let Some(sub_spec) = spec.subcommands.get(&sub.to_ascii_uppercase())
        {
            return Some((sub_spec, 2));
        }
        Some((spec, 1))
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// The compiled-in snapshot, parsed once per process.
pub fn bundled() -> &'static CommandVocabulary {
    static BUNDLED: OnceLock<CommandVocabulary> = OnceLock::new();
    BUNDLED.get_or_init(|| {
        let value: Value =
            serde_json::from_str(BUNDLED_JSON).expect("bundled redis commands.json is valid JSON");
        let (docs, info) = match value {
            Value::Object(mut map) => (
                map.remove("docs").unwrap_or(Value::Null),
                map.remove("info").unwrap_or(Value::Null),
            ),
            _ => (Value::Null, Value::Null),
        };
        CommandVocabulary::from_json(docs, info, VocabularySource::Bundled)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::safety::redis::{DESTRUCTIVE_COMMANDS, READ_ONLY_COMMANDS};

    #[test]
    fn bundled_snapshot_parses_with_docs_and_info() {
        let vocab = bundled();
        assert!(vocab.len() > 400, "{}", vocab.len());
        let zrange = vocab.get("zrange").expect("ZRANGE");
        assert_eq!(
            zrange.syntax(),
            "ZRANGE key start stop [BYSCORE | BYLEX] [REV] [LIMIT offset count] [WITHSCORES]"
        );
        assert!(zrange.info.has_flag("readonly"));
        let config = vocab.get("CONFIG").expect("CONFIG");
        assert!(config.is_container());
        assert_eq!(config.subcommands["GET"].name, "CONFIG GET");
        assert!(config.subcommands["SET"].info.has_flag("admin"));
        // Module commands registered without docs got hand-written ones.
        let search = vocab.get("FT.SEARCH").expect("FT.SEARCH");
        assert!(!search.summary.is_empty());
        assert!(search.syntax().starts_with("FT.SEARCH index query"));
        assert!(vocab.get("JSON.GET").is_some());
        assert!(vocab.get("SQLLY.KEYS").is_some());
    }

    /// The safety allowlists and the vocabulary must agree: a command one
    /// knows and the other does not is noticed here, not by a user.
    #[test]
    fn bundled_vocabulary_covers_every_safety_listed_command() {
        let vocab = bundled();
        let missing: Vec<&str> = READ_ONLY_COMMANDS
            .iter()
            .chain(DESTRUCTIVE_COMMANDS.iter())
            .copied()
            .filter(|name| vocab.get(name).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "safety lists name commands the vocabulary lacks: {missing:?}"
        );
        // And the other way: nothing in the read-only list is flagged as a
        // write by the server's own COMMAND INFO.
        let disagreements: Vec<&str> = READ_ONLY_COMMANDS
            .iter()
            .copied()
            .filter(|name| {
                vocab
                    .get(name)
                    .is_some_and(|spec| spec.info.has_flag("write"))
            })
            .collect();
        assert!(
            disagreements.is_empty(),
            "read-only list names commands COMMAND INFO flags as write: {disagreements:?}"
        );
    }

    #[test]
    fn server_rows_replace_the_bundle_but_keep_virtual_commands() {
        let rows = vec![
            (
                "get",
                r#"["summary","Returns the string value of a key.","since","1.0.0","group","string","arguments",[["name","key","type","key"]]]"#,
                r#"["get",2,["readonly","fast"],1,1,1,["@read"],[],[[]],[]]"#,
            ),
            (
                "ping",
                "{}",
                r#"["ping",-1,["fast"],0,0,0,["@fast"],[],[],[]]"#,
            ),
        ];
        let vocab = CommandVocabulary::from_server_rows(rows).expect("rows");
        assert_eq!(vocab.source, VocabularySource::Server { command_count: 2 });
        assert!(vocab.get("GET").is_some());
        assert!(vocab.get("PING").is_some());
        assert!(vocab.get("HSET").is_none(), "server did not list HSET");
        assert!(vocab.get("SQLLY.KEYS").is_some());
        assert!(CommandVocabulary::from_server_rows(Vec::new()).is_none());
    }

    #[test]
    fn badges_follow_the_safety_classification() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            badges_for(&s(&["FLUSHALL"])),
            vec![CommandBadge::Destructive]
        );
        assert_eq!(
            badges_for(&s(&["SET", "k", "v"])),
            vec![CommandBadge::Write]
        );
        assert!(badges_for(&s(&["GET", "k"])).is_empty());
        let blpop = badges_for(&s(&["BLPOP", "q", "0"]));
        assert!(matches!(blpop[1], CommandBadge::Blocking(_)), "{blpop:?}");
        assert!(badges_for(&s(&["BLPOP", "q", "5"])).len() == 1);
        assert!(
            badges_for(&s(&["SUBSCRIBE", "news"]))
                .iter()
                .any(|b| matches!(b, CommandBadge::Blocking(_)))
        );
    }
}
