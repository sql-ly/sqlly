//! The command vocabulary: `COMMAND DOCS` + `COMMAND INFO` parsed into
//! [`CommandSpec`]s.
//!
//! Both replies arrive in one of two JSON spellings. `redis-cli --json` (and
//! a RESP3 connection) renders the map-shaped docs as objects; the driver's
//! RESP2 connection renders the same maps as flat `[key, value, key, value]`
//! arrays. [`normalize_docs`] folds the second spelling into the first, so
//! the parser sees one shape, and every name is upper-cased so `get`,
//! `GET` and the module-registered `FT.SEARCH` share one spelling.

use std::collections::BTreeMap;

use serde_json::Value;

/// What an argument position accepts, from `COMMAND DOCS`'s `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgType {
    /// A key name — the slot key-name completion fills.
    Key,
    String,
    Integer,
    Double,
    UnixTime,
    Pattern,
    /// A fixed word (`WITHSCORES`); `token` carries its spelling.
    PureToken,
    /// Exactly one of `arguments` (`BYSCORE | BYLEX`).
    OneOf,
    /// `arguments` in sequence, optionally introduced by `token` (`LIMIT offset count`).
    Block,
}

impl ArgType {
    fn parse(text: &str) -> ArgType {
        match text {
            "key" => ArgType::Key,
            "integer" => ArgType::Integer,
            "double" => ArgType::Double,
            "unix-time" => ArgType::UnixTime,
            "pattern" => ArgType::Pattern,
            "pure-token" => ArgType::PureToken,
            "oneof" => ArgType::OneOf,
            "block" => ArgType::Block,
            _ => ArgType::String,
        }
    }

    /// The word a hover or signature shows for the slot's type.
    pub fn label(self) -> &'static str {
        match self {
            ArgType::Key => "key",
            ArgType::String => "string",
            ArgType::Integer => "integer",
            ArgType::Double => "double",
            ArgType::UnixTime => "unix time",
            ArgType::Pattern => "pattern",
            ArgType::PureToken => "token",
            ArgType::OneOf => "one of",
            ArgType::Block => "block",
        }
    }
}

/// One argument of a command's documented grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    pub name: String,
    pub arg_type: ArgType,
    /// What the docs print for the slot (`key`, `start`), falling back to `name`.
    pub display: String,
    /// The fixed word that introduces the argument (`LIMIT`, `WITHSCORES`).
    pub token: Option<String>,
    pub optional: bool,
    pub multiple: bool,
    /// `multiple_token`: the token is repeated before every repetition.
    pub multiple_token: bool,
    /// Children of a `OneOf` or `Block`.
    pub arguments: Vec<Arg>,
    pub since: Option<String>,
    pub deprecated_since: Option<String>,
}

impl Arg {
    /// Every fixed word this argument (or one of its alternatives) can start
    /// with: the token of a token-bearing argument, the tokens of a `OneOf`'s
    /// alternatives, and the token of a `Block`.
    pub fn tokens(&self) -> Vec<&str> {
        let mut out = Vec::new();
        if let Some(token) = self.token.as_deref() {
            out.push(token);
        } else if self.arg_type == ArgType::OneOf {
            for child in &self.arguments {
                out.extend(child.tokens());
            }
        }
        out
    }

    /// True when the argument begins with a fixed word, so its presence in
    /// a typed line is decided by that word.
    pub fn is_token_bearing(&self) -> bool {
        !self.tokens().is_empty()
    }

    /// The documented rendering: `key`, `[WITHSCORES]`, `[BYSCORE | BYLEX]`,
    /// `[LIMIT offset count]`, `member [member ...]`.
    pub fn syntax(&self) -> String {
        let core = self.syntax_core();
        let mut text = if self.multiple {
            if self.multiple_token {
                format!("{core} [{core} ...]")
            } else {
                let repeat = self.syntax_core_without_token();
                format!("{core} [{repeat} ...]")
            }
        } else {
            core
        };
        if self.optional {
            text = format!("[{text}]");
        }
        text
    }

    fn syntax_core(&self) -> String {
        match self.arg_type {
            ArgType::PureToken => self.token.clone().unwrap_or_else(|| self.display.clone()),
            ArgType::OneOf => {
                let inner = self
                    .arguments
                    .iter()
                    .map(Arg::syntax)
                    .collect::<Vec<_>>()
                    .join(" | ");
                match &self.token {
                    Some(token) => format!("{token} <{inner}>"),
                    None if self.optional => inner,
                    None => format!("<{inner}>"),
                }
            }
            ArgType::Block => {
                let inner = self
                    .arguments
                    .iter()
                    .map(Arg::syntax)
                    .collect::<Vec<_>>()
                    .join(" ");
                match &self.token {
                    Some(token) => format!("{token} {inner}"),
                    None => inner,
                }
            }
            _ => match &self.token {
                Some(token) => format!("{token} {}", self.display),
                None => self.display.clone(),
            },
        }
    }

    fn syntax_core_without_token(&self) -> String {
        match self.arg_type {
            ArgType::Block | ArgType::OneOf if self.token.is_some() => {
                let mut copy = self.clone();
                copy.token = None;
                copy.syntax_core()
            }
            _ => self.syntax_core(),
        }
    }
}

/// How `COMMAND INFO` classifies a command, kept for the hover badges and
/// for the agreement test against the safety lists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandInfo {
    /// Positive: exact argument count including the name; negative: at least
    /// `-arity` arguments. `0` when unknown.
    pub arity: i64,
    /// `readonly`, `write`, `admin`, `blocking`, `module`, …
    pub flags: Vec<String>,
    /// `@read`, `@write`, `@dangerous`, …
    pub acl_categories: Vec<String>,
    /// How many key specs the command declares (0 for keyless commands).
    pub key_spec_count: usize,
}

impl CommandInfo {
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case(flag))
    }
}

/// One command (or one subcommand of a container, spelled `CONFIG GET`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Upper-case name: `HSET`, `JSON.GET`, `CONFIG GET` for a subcommand.
    pub name: String,
    pub summary: String,
    pub since: Option<String>,
    pub group: String,
    pub complexity: Option<String>,
    pub deprecated_since: Option<String>,
    pub arguments: Vec<Arg>,
    /// `CONFIG`'s `GET`, `SET`, …; empty for ordinary commands.
    pub subcommands: BTreeMap<String, CommandSpec>,
    pub info: CommandInfo,
}

impl CommandSpec {
    pub fn is_container(&self) -> bool {
        !self.subcommands.is_empty()
    }

    /// The documented one-line syntax: `ZRANGE key start stop [BYSCORE | BYLEX] …`.
    pub fn syntax(&self) -> String {
        let mut parts = vec![self.name.clone()];
        parts.extend(self.arguments.iter().map(Arg::syntax));
        parts.join(" ")
    }

    /// True when `COMMAND INFO` marks the command read-only (or its ACL
    /// categories do, for modules that set no flags).
    pub fn info_says_read_only(&self) -> bool {
        self.info.has_flag("readonly")
            || (!self.info.has_flag("write")
                && self
                    .info
                    .acl_categories
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case("@read")))
    }
}

/// Fold a RESP2 `[key, value, …]` pair array into an object. Objects and
/// scalars pass through; arrays whose even positions are not all strings
/// are left alone (they are real lists).
pub fn pairs_to_object(value: Value) -> Value {
    match value {
        Value::Array(items)
            if !items.is_empty()
                && items.len() % 2 == 0
                && items.iter().step_by(2).all(Value::is_string) =>
        {
            let mut map = serde_json::Map::with_capacity(items.len() / 2);
            let mut iter = items.into_iter();
            while let (Some(Value::String(key)), Some(val)) = (iter.next(), iter.next()) {
                map.insert(key, val);
            }
            Value::Object(map)
        }
        other => other,
    }
}

/// Normalize one command's docs body (object or pair array) into an object
/// whose `subcommands` map and `arguments` tree are objects too.
pub fn normalize_doc_body(value: Value) -> Value {
    let Value::Object(mut map) = pairs_to_object(value) else {
        return Value::Object(serde_json::Map::new());
    };
    if let Some(subs) = map.remove("subcommands") {
        let Value::Object(subs) = pairs_to_object(subs) else {
            return Value::Object(map);
        };
        let normalized: serde_json::Map<String, Value> = subs
            .into_iter()
            .map(|(name, body)| (name, normalize_doc_body(body)))
            .collect();
        map.insert("subcommands".into(), Value::Object(normalized));
    }
    if let Some(args) = map.remove("arguments") {
        map.insert("arguments".into(), normalize_arguments(args));
    }
    Value::Object(map)
}

fn normalize_arguments(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| {
                    let Value::Object(mut map) = pairs_to_object(item) else {
                        return Value::Null;
                    };
                    if let Some(nested) = map.remove("arguments") {
                        map.insert("arguments".into(), normalize_arguments(nested));
                    }
                    Value::Object(map)
                })
                .collect(),
        ),
        other => other,
    }
}

/// Normalize a whole `COMMAND DOCS` reply (object keyed by name, or the
/// RESP2 `[name, body, name, body]` array) into `NAME → body` with
/// upper-case names and normalized bodies.
pub fn normalize_docs(value: Value) -> BTreeMap<String, Value> {
    let Value::Object(map) = pairs_to_object(value) else {
        return BTreeMap::new();
    };
    map.into_iter()
        .map(|(name, body)| (name.to_ascii_uppercase(), normalize_doc_body(body)))
        .collect()
}

/// Normalize a `COMMAND INFO` reply (array of rows, or an object keyed by
/// name) into `NAME → row` with upper-case names.
pub fn normalize_info(value: Value) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    match value {
        Value::Object(map) => {
            for (name, row) in map {
                out.insert(name.to_ascii_uppercase(), row);
            }
        }
        Value::Array(rows) => {
            for row in rows {
                if let Value::Array(items) = &row
                    && let Some(Value::String(name)) = items.first()
                {
                    out.insert(name.to_ascii_uppercase(), row);
                }
            }
        }
        _ => {}
    }
    out
}

fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_string)
}

fn flags_field(map: &serde_json::Map<String, Value>) -> Vec<String> {
    map.get("flags")
        .and_then(Value::as_array)
        .map(|flags| {
            flags
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_arg(value: &Value) -> Option<Arg> {
    let map = value.as_object()?;
    let name = string_field(map, "name").unwrap_or_default();
    let arg_type = ArgType::parse(&string_field(map, "type").unwrap_or_default());
    let flags = flags_field(map);
    let has = |flag: &str| flags.iter().any(|f| f == flag);
    let arguments = map
        .get("arguments")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_arg).collect())
        .unwrap_or_default();
    let display = string_field(map, "display_text").unwrap_or_else(|| name.clone());
    Some(Arg {
        display,
        name,
        arg_type,
        token: string_field(map, "token"),
        optional: has("optional"),
        multiple: has("multiple"),
        multiple_token: has("multiple_token"),
        arguments,
        since: string_field(map, "since"),
        deprecated_since: string_field(map, "deprecated_since"),
    })
}

fn parse_info_row(row: &Value) -> CommandInfo {
    let Some(items) = row.as_array() else {
        return CommandInfo::default();
    };
    let strings = |index: usize| -> Vec<String> {
        items
            .get(index)
            .and_then(Value::as_array)
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    CommandInfo {
        arity: items.get(1).and_then(Value::as_i64).unwrap_or(0),
        flags: strings(2),
        acl_categories: strings(6),
        key_spec_count: items
            .get(8)
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
    }
}

/// The `COMMAND INFO` rows of a container's subcommands, keyed by the
/// `CONTAINER|SUB` spelling the server uses.
fn info_subcommand_rows(row: &Value) -> BTreeMap<String, Value> {
    row.as_array()
        .and_then(|items| items.get(9))
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let name = r.as_array()?.first()?.as_str()?;
                    Some((name.to_ascii_uppercase(), r.clone()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse one normalized docs body plus its info row into a spec. `name` is
/// the full upper-case spelling (`CONFIG GET` for a subcommand).
pub fn parse_spec(name: &str, body: &Value, info_row: Option<&Value>) -> CommandSpec {
    let empty = serde_json::Map::new();
    let map = body.as_object().unwrap_or(&empty);
    let arguments = map
        .get("arguments")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_arg).collect())
        .unwrap_or_default();
    let sub_info = info_row.map(info_subcommand_rows).unwrap_or_default();
    let subcommands = map
        .get("subcommands")
        .and_then(Value::as_object)
        .map(|subs| {
            subs.iter()
                .map(|(sub_name, sub_body)| {
                    let upper = sub_name.to_ascii_uppercase();
                    let short = upper
                        .rsplit('|')
                        .next()
                        .unwrap_or(upper.as_str())
                        .to_string();
                    let full = format!("{name} {short}");
                    let spec = parse_spec(&full, sub_body, sub_info.get(&upper));
                    (short, spec)
                })
                .collect()
        })
        .unwrap_or_default();
    let deprecated_since = string_field(map, "deprecated_since").or_else(|| {
        map.get("doc_flags")
            .and_then(Value::as_array)
            .and_then(|flags| {
                flags
                    .iter()
                    .any(|f| f.as_str() == Some("deprecated"))
                    .then(|| "deprecated".to_string())
            })
    });
    CommandSpec {
        name: name.to_string(),
        summary: string_field(map, "summary").unwrap_or_default(),
        since: string_field(map, "since"),
        group: string_field(map, "group").unwrap_or_else(|| "other".to_string()),
        complexity: string_field(map, "complexity"),
        deprecated_since,
        arguments,
        subcommands,
        info: info_row.map(parse_info_row).unwrap_or_default(),
    }
}

/// Parse a normalized `COMMAND DOCS` map and `COMMAND INFO` map into specs
/// keyed by upper-case name. Commands that only `COMMAND INFO` knows (docs
/// refused, or a module with no docs) still get a spec with an empty
/// summary so they complete.
pub fn parse_vocabulary(
    docs: &BTreeMap<String, Value>,
    info: &BTreeMap<String, Value>,
) -> BTreeMap<String, CommandSpec> {
    let mut out: BTreeMap<String, CommandSpec> = docs
        .iter()
        .map(|(name, body)| (name.clone(), parse_spec(name, body, info.get(name))))
        .collect();
    for (name, row) in info {
        if !out.contains_key(name) {
            out.insert(name.clone(), parse_spec(name, &Value::Null, Some(row)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resp2_pair_arrays_fold_into_the_resp3_object_shape() {
        let resp2: Value = serde_json::from_str(
            r#"["zrange",["summary","Returns members.","group","sorted-set","arguments",[["name","key","type","key","display_text","key","key_spec_index",0],["name","sortby","type","oneof","flags",["optional"],"arguments",[["name","byscore","type","pure-token","token","BYSCORE"]]]]]]"#,
        )
        .unwrap();
        let resp3: Value = serde_json::from_str(
            r#"{"ZRANGE":{"summary":"Returns members.","group":"sorted-set","arguments":[{"name":"key","type":"key","display_text":"key","key_spec_index":0},{"name":"sortby","type":"oneof","flags":["optional"],"arguments":[{"name":"byscore","type":"pure-token","token":"BYSCORE"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(normalize_docs(resp2), normalize_docs(resp3));
    }

    #[test]
    fn info_rows_parse_arity_flags_and_subcommands() {
        let info: Value = serde_json::from_str(
            r#"[["config",-2,[],0,0,0,["@slow"],[],[],[["config|get",-3,["admin","loading"],0,0,0,["@admin","@slow"],[],[],[]]]]]"#,
        )
        .unwrap();
        let info = normalize_info(info);
        let docs: Value = serde_json::from_str(
            r#"{"config":{"summary":"A container.","group":"server","subcommands":{"config|get":{"summary":"Returns values.","group":"server","arguments":[{"name":"parameter","type":"string","flags":["multiple"]}]}}}}"#,
        )
        .unwrap();
        let docs = normalize_docs(docs);
        let specs = parse_vocabulary(&docs, &info);
        let config = &specs["CONFIG"];
        assert!(config.is_container());
        assert_eq!(config.info.arity, -2);
        let get = &config.subcommands["GET"];
        assert_eq!(get.name, "CONFIG GET");
        assert!(get.info.has_flag("admin"));
        assert_eq!(get.syntax(), "CONFIG GET parameter [parameter ...]");
    }

    #[test]
    fn syntax_renders_like_the_redis_docs() {
        let docs: Value = serde_json::from_str(
            r#"{"ZRANGE":{"summary":"s","group":"sorted-set","arguments":[{"name":"key","type":"key"},{"name":"start","type":"string"},{"name":"stop","type":"string"},{"name":"sortby","type":"oneof","flags":["optional"],"arguments":[{"name":"byscore","type":"pure-token","token":"BYSCORE"},{"name":"bylex","type":"pure-token","token":"BYLEX"}]},{"name":"rev","type":"pure-token","token":"REV","flags":["optional"]},{"name":"limit","type":"block","token":"LIMIT","flags":["optional"],"arguments":[{"name":"offset","type":"integer"},{"name":"count","type":"integer"}]},{"name":"withscores","type":"pure-token","token":"WITHSCORES","flags":["optional"]}]},
                "HSET":{"summary":"s","group":"hash","arguments":[{"name":"key","type":"key"},{"name":"data","type":"block","flags":["multiple"],"arguments":[{"name":"field","type":"string"},{"name":"value","type":"string"}]}]},
                "SET":{"summary":"s","group":"string","arguments":[{"name":"key","type":"key"},{"name":"value","type":"string"},{"name":"condition","type":"oneof","flags":["optional"],"arguments":[{"name":"nx","type":"pure-token","token":"NX"},{"name":"xx","type":"pure-token","token":"XX"}]},{"name":"expiration","type":"oneof","flags":["optional"],"arguments":[{"name":"seconds","type":"integer","token":"EX"},{"name":"keepttl","type":"pure-token","token":"KEEPTTL"}]}]}}"#,
        )
        .unwrap();
        let specs = parse_vocabulary(&normalize_docs(docs), &BTreeMap::new());
        assert_eq!(
            specs["ZRANGE"].syntax(),
            "ZRANGE key start stop [BYSCORE | BYLEX] [REV] [LIMIT offset count] [WITHSCORES]"
        );
        assert_eq!(
            specs["HSET"].syntax(),
            "HSET key field value [field value ...]"
        );
        assert_eq!(
            specs["SET"].syntax(),
            "SET key value [NX | XX] [EX seconds | KEEPTTL]"
        );
        assert_eq!(specs["SET"].arguments[3].tokens(), vec!["EX", "KEEPTTL"]);
    }
}
