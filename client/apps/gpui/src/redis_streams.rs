//! The key view's stream tabs, the part that is not drawing: which tab is
//! showing, the readers each tab runs (`SQLLY.XPAGE` for entries,
//! `SQLLY.XGROUPS`, `XINFO CONSUMERS`, `SQLLY.XPENDING`), the group and
//! pending rows read back by column name, and the stream actions — `XADD`,
//! `XDEL`, `XACK`, `XCLAIM`, `XAUTOCLAIM`, `XGROUP CREATE` / `DESTROY`,
//! `XTRIM` — each producing ONE exact command line.
//!
//! Field names, values, group and consumer names are `Vec<u8>` from the
//! grid row (or the typed input) to the command text, quoted with the same
//! `quote_argument` the script tokenizer decodes byte for byte. Counts and
//! idle thresholds are command arguments, not values: they are checked to
//! be plain digits and passed on as typed, never re-formatted.
//!
//! `XTRIM` and `XGROUP DESTROY` lose data the server cannot give back, so on
//! top of the key view's review they ask the user to type the trim
//! threshold or the group's name before Run is allowed.

use sqlly_core::CellValue;
use sqlly_intel::query::safety::redis::quote_argument;

use crate::tool_dialog::ToolQuerySet;

/// Entries `XAUTOCLAIM` moves in one run.
pub const AUTOCLAIM_COUNT: usize = 100;
/// The idle threshold Auto-claim suggests: a minute without acknowledging.
pub const DEFAULT_AUTOCLAIM_IDLE_MS: &str = "60000";
/// Pending entries one page of the Pending tab loads.
pub const PENDING_PAGE: usize = 1000;

/// The tabs a stream key shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StreamTab {
    #[default]
    Entries,
    Groups,
    Consumers,
    Pending,
}

impl StreamTab {
    pub const ALL: [StreamTab; 4] = [
        StreamTab::Entries,
        StreamTab::Groups,
        StreamTab::Consumers,
        StreamTab::Pending,
    ];

    pub fn label(self) -> String {
        crate::localization::text(match self {
            Self::Entries => "explorer.redis_streams.tab_entries",
            Self::Groups => "explorer.redis_streams.tab_groups",
            Self::Consumers => "explorer.redis_streams.tab_consumers",
            Self::Pending => "explorer.redis_streams.tab_pending",
        })
    }

    /// Whether the tab is about one consumer group.
    pub fn needs_group(self) -> bool {
        matches!(self, Self::Consumers | Self::Pending)
    }
}

/// `SQLLY.XGROUPS <key>`.
pub fn groups_command(key: &[u8]) -> String {
    format!("SQLLY.XGROUPS {}", quote_argument(key))
}

/// `XINFO CONSUMERS <key> <group>` (shaped by the shared `XINFO` shape).
pub fn consumers_command(key: &[u8], group: &[u8]) -> String {
    format!(
        "XINFO CONSUMERS {} {}",
        quote_argument(key),
        quote_argument(group)
    )
}

/// `SQLLY.XPENDING <key> <group> <from> <count>`, continuing at `next`.
pub fn pending_command(key: &[u8], group: &[u8], next: Option<&str>, count: usize) -> String {
    format!(
        "SQLLY.XPENDING {} {} {} {count}",
        quote_argument(key),
        quote_argument(group),
        next.unwrap_or("-")
    )
}

fn column_index(set: &ToolQuerySet, name: &str) -> Option<usize> {
    set.columns.iter().position(|c| c.name == name)
}

fn bytes_of(cell: Option<&CellValue>) -> Option<Vec<u8>> {
    match cell? {
        CellValue::String(s) => Some(s.as_bytes().to_vec()),
        CellValue::Bytes(b) => Some(b.clone()),
        _ => None,
    }
}

fn int_of(cell: Option<&CellValue>) -> Option<i64> {
    match cell? {
        CellValue::I64(n) => Some(*n),
        _ => None,
    }
}

/// One consumer group as `SQLLY.XGROUPS` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupRow {
    /// The group's exact name.
    pub name: Vec<u8>,
    pub consumers: Option<i64>,
    pub pending: Option<i64>,
    pub last_delivered_id: Option<String>,
    /// `None` before Redis 7, or when the server cannot tell.
    pub lag: Option<i64>,
}

/// The groups in a `SQLLY.XGROUPS` result, read by column name.
pub fn parse_groups(set: &ToolQuerySet) -> Vec<GroupRow> {
    let Some(name) = column_index(set, "group") else {
        return Vec::new();
    };
    let at = |row: &Vec<CellValue>, column: &str| {
        column_index(set, column).and_then(|i| row.get(i)).cloned()
    };
    set.rows
        .iter()
        .filter_map(|row| {
            Some(GroupRow {
                name: bytes_of(row.get(name))?,
                consumers: int_of(at(row, "consumers").as_ref()),
                pending: int_of(at(row, "pending").as_ref()),
                last_delivered_id: match at(row, "last_delivered_id") {
                    Some(CellValue::String(s)) => Some(s),
                    _ => None,
                },
                lag: int_of(at(row, "lag").as_ref()),
            })
        })
        .collect()
}

/// One pending entry as `SQLLY.XPENDING` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingRow {
    pub id: Vec<u8>,
    pub consumer: Vec<u8>,
    pub idle_ms: Option<i64>,
    pub deliveries: Option<i64>,
}

/// The pending row at `index` of a pending page (data columns only).
pub fn pending_row(columns: &[sqlly_core::ResultColumn], row: &[CellValue]) -> Option<PendingRow> {
    let at = |name: &str| {
        columns
            .iter()
            .position(|c| c.name == name)
            .and_then(|i| row.get(i))
    };
    Some(PendingRow {
        id: bytes_of(at("id"))?,
        consumer: bytes_of(at("consumer"))?,
        idle_ms: int_of(at("idle_ms")),
        deliveries: int_of(at("deliveries")),
    })
}

/// How `XTRIM` decides what to keep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrimStrategy {
    /// Keep the newest N entries.
    #[default]
    MaxLen,
    /// Drop entries with ids below this one.
    MinId,
}

impl TrimStrategy {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::MaxLen => "MAXLEN",
            Self::MinId => "MINID",
        }
    }
}

/// One stream action: exactly one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamAction {
    /// `XADD key <id> field value …` (`*` lets the server pick the id).
    Append {
        id: Vec<u8>,
        pairs: Vec<(Vec<u8>, Vec<u8>)>,
    },
    /// `XDEL key id`.
    Delete { id: Vec<u8> },
    /// `XACK key group id`.
    Ack { group: Vec<u8>, id: Vec<u8> },
    /// `XCLAIM key group consumer min-idle-ms id`.
    Claim {
        group: Vec<u8>,
        consumer: Vec<u8>,
        min_idle_ms: String,
        id: Vec<u8>,
    },
    /// `XAUTOCLAIM key group consumer min-idle-ms start COUNT n`.
    AutoClaim {
        group: Vec<u8>,
        consumer: Vec<u8>,
        min_idle_ms: String,
        start: Vec<u8>,
        count: usize,
    },
    /// `XGROUP CREATE key group <id>` (`$` = only new entries, `0` = all).
    GroupCreate { group: Vec<u8>, start: Vec<u8> },
    /// `XGROUP DESTROY key group`.
    GroupDestroy { group: Vec<u8> },
    /// `XTRIM key MAXLEN|MINID [~] threshold`.
    Trim {
        strategy: TrimStrategy,
        approximate: bool,
        threshold: Vec<u8>,
    },
}

impl StreamAction {
    /// The exact command line, every argument quoted byte-exact (stream
    /// ids, which are plain ASCII, are written as they are).
    pub fn command(&self, key: &[u8]) -> String {
        let k = quote_argument(key);
        let q = |b: &[u8]| quote_argument(b);
        let id_arg = |b: &[u8]| {
            if !b.is_empty() && b.iter().all(|c| c.is_ascii_digit() || b"-*$()".contains(c)) {
                String::from_utf8_lossy(b).into_owned()
            } else {
                quote_argument(b)
            }
        };
        match self {
            Self::Append { id, pairs } => {
                let mut text = format!("XADD {k} {}", id_arg(id));
                for (field, value) in pairs {
                    text.push(' ');
                    text.push_str(&q(field));
                    text.push(' ');
                    text.push_str(&q(value));
                }
                text
            }
            Self::Delete { id } => format!("XDEL {k} {}", id_arg(id)),
            Self::Ack { group, id } => format!("XACK {k} {} {}", q(group), id_arg(id)),
            Self::Claim {
                group,
                consumer,
                min_idle_ms,
                id,
            } => format!(
                "XCLAIM {k} {} {} {min_idle_ms} {}",
                q(group),
                q(consumer),
                id_arg(id)
            ),
            Self::AutoClaim {
                group,
                consumer,
                min_idle_ms,
                start,
                count,
            } => format!(
                "XAUTOCLAIM {k} {} {} {min_idle_ms} {} COUNT {count}",
                q(group),
                q(consumer),
                id_arg(start)
            ),
            Self::GroupCreate { group, start } => {
                format!("XGROUP CREATE {k} {} {}", q(group), id_arg(start))
            }
            Self::GroupDestroy { group } => format!("XGROUP DESTROY {k} {}", q(group)),
            Self::Trim {
                strategy,
                approximate,
                threshold,
            } => format!(
                "XTRIM {k} {}{} {}",
                strategy.keyword(),
                if *approximate { " ~" } else { "" },
                id_arg(threshold)
            ),
        }
    }

    /// What the user must type before Run is allowed, for the actions that
    /// lose data for good: the trim threshold, or the group's name.
    pub fn typed_confirmation(&self) -> Option<String> {
        match self {
            Self::Trim { threshold, .. } => Some(String::from_utf8_lossy(threshold).into_owned()),
            Self::GroupDestroy { group } => Some(crate::redis_key_view::editor_text_for(group).0),
            _ => None,
        }
    }

    /// A caveat the review shows beside the command.
    pub fn note(&self) -> Option<String> {
        let key = match self {
            Self::Delete { .. } => "explorer.redis_streams.note_delete",
            Self::Ack { .. } => "explorer.redis_streams.note_ack",
            Self::Claim { .. } => "explorer.redis_streams.note_claim",
            Self::AutoClaim { .. } => "explorer.redis_streams.note_auto_claim",
            Self::GroupCreate { .. } => "explorer.redis_streams.note_group_create",
            Self::GroupDestroy { .. } => "explorer.redis_streams.note_group_destroy",
            Self::Trim {
                strategy: TrimStrategy::MaxLen,
                ..
            } => "explorer.redis_streams.note_trim_maxlen",
            Self::Trim {
                strategy: TrimStrategy::MinId,
                ..
            } => "explorer.redis_streams.note_trim_minid",
            Self::Append { .. } => return None,
        };
        Some(crate::localization::text(key))
    }
}

/// `true` when the typed text matches what the action asks for (surrounding
/// whitespace ignored, nothing else).
pub fn typed_confirmation_matches(expected: &str, typed: &str) -> bool {
    !expected.is_empty() && typed.trim() == expected
}

/// A count or millisecond argument: plain digits only, kept as typed.
pub fn digits(text: &str, what: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(crate::localization::format(
            "explorer.redis_streams.invalid_whole_number",
            &[("field", what)],
        ));
    }
    Ok(text.to_string())
}

/// A stream id argument: `ms-seq`, `ms`, or one of the specials the
/// action allows (`*` for XADD, `$` / `0` for XGROUP CREATE, `0-0` start).
pub fn stream_id(text: &str, specials: &[&str]) -> Result<Vec<u8>, String> {
    let text = text.trim();
    if specials.contains(&text) {
        return Ok(text.as_bytes().to_vec());
    }
    let mut parts = text.splitn(2, '-');
    let ms = parts.next().unwrap_or("");
    let seq = parts.next();
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let ok = numeric(ms) && seq.is_none_or(|s| numeric(s) || s == "*");
    if ok {
        Ok(text.as_bytes().to_vec())
    } else if specials.is_empty() {
        Err(crate::localization::format(
            "explorer.redis_streams.invalid_stream_id",
            &[("value", &format!("{text:?}"))],
        ))
    } else {
        Err(crate::localization::format(
            "explorer.redis_streams.invalid_stream_id_with_specials",
            &[
                ("value", &format!("{text:?}")),
                ("specials", &specials.join(" ")),
            ],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::query::safety::redis::parse_script;

    fn column(name: &str) -> sqlly_core::ResultColumn {
        sqlly_core::ResultColumn {
            name: name.to_string(),
            data_type: String::new(),
            nullable: true,
        }
    }

    #[test]
    fn redis_streams_readers_quote_keys_and_groups_byte_exact() {
        assert_eq!(groups_command(b"wsg:x"), "SQLLY.XGROUPS wsg:x");
        assert_eq!(
            consumers_command(b"wsg:x", b"two words"),
            "XINFO CONSUMERS wsg:x \"two words\""
        );
        assert_eq!(
            pending_command(b"wsg:x", b"g", None, 1000),
            "SQLLY.XPENDING wsg:x g - 1000"
        );
        assert_eq!(
            pending_command(b"wsg:x", &[0xff], Some("(5-0"), 10),
            "SQLLY.XPENDING wsg:x \"\\xff\" (5-0 10"
        );
        for text in [
            consumers_command(b"k", &[0xff, 0x00]),
            pending_command(b"k", &[0xff, 0x00], None, 5),
        ] {
            let commands = parse_script(&text).unwrap();
            assert_eq!(
                commands[0].args[if text.starts_with("XINFO") { 3 } else { 2 }],
                vec![0xff, 0x00]
            );
        }
        assert!(StreamTab::Pending.needs_group());
        assert!(!StreamTab::Groups.needs_group());
    }

    #[test]
    fn redis_streams_parse_groups_and_pending_by_name() {
        let set = ToolQuerySet {
            columns: [
                "group",
                "consumers",
                "pending",
                "last_delivered_id",
                "entries_read",
                "lag",
                "stream_length",
            ]
            .iter()
            .map(|n| column(n))
            .collect(),
            rows: vec![
                vec![
                    CellValue::String("workers".into()),
                    CellValue::I64(2),
                    CellValue::I64(1),
                    CellValue::String("5-0".into()),
                    CellValue::I64(5),
                    CellValue::I64(3),
                    CellValue::I64(8),
                ],
                vec![
                    CellValue::Bytes(vec![0xff]),
                    CellValue::I64(0),
                    CellValue::I64(0),
                    CellValue::String("0-0".into()),
                    CellValue::Null,
                    CellValue::Null,
                    CellValue::I64(8),
                ],
            ],
        };
        let groups = parse_groups(&set);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, b"workers");
        assert_eq!(groups[0].lag, Some(3));
        assert_eq!(groups[0].last_delivered_id.as_deref(), Some("5-0"));
        assert_eq!(groups[1].name, vec![0xff]);
        assert_eq!(groups[1].lag, None);
        assert!(parse_groups(&ToolQuerySet::default()).is_empty());

        let columns: Vec<_> = ["id", "consumer", "idle_ms", "deliveries"]
            .iter()
            .map(|n| column(n))
            .collect();
        let row = pending_row(
            &columns,
            &[
                CellValue::String("1-0".into()),
                CellValue::String("dead".into()),
                CellValue::I64(93_000),
                CellValue::I64(1),
            ],
        )
        .unwrap();
        assert_eq!(row.id, b"1-0");
        assert_eq!(row.consumer, b"dead");
        assert_eq!(row.idle_ms, Some(93_000));
        assert!(pending_row(&columns, &[CellValue::Null]).is_none());
    }

    #[test]
    fn redis_streams_commands_for_every_action() {
        let k = b"wsg:x".as_slice();
        let g = b"workers".to_vec();
        let cases: Vec<(StreamAction, &str)> = vec![
            (
                StreamAction::Append {
                    id: b"*".to_vec(),
                    pairs: vec![
                        (b"n".to_vec(), b"00014".to_vec()),
                        (b"msg".to_vec(), b"two words".to_vec()),
                    ],
                },
                "XADD wsg:x * n 00014 msg \"two words\"",
            ),
            (
                StreamAction::Delete {
                    id: b"1-0".to_vec(),
                },
                "XDEL wsg:x 1-0",
            ),
            (
                StreamAction::Ack {
                    group: g.clone(),
                    id: b"1-0".to_vec(),
                },
                "XACK wsg:x workers 1-0",
            ),
            (
                StreamAction::Claim {
                    group: g.clone(),
                    consumer: b"alive".to_vec(),
                    min_idle_ms: "93000".into(),
                    id: b"1-0".to_vec(),
                },
                "XCLAIM wsg:x workers alive 93000 1-0",
            ),
            (
                StreamAction::AutoClaim {
                    group: g.clone(),
                    consumer: b"alive".to_vec(),
                    min_idle_ms: DEFAULT_AUTOCLAIM_IDLE_MS.into(),
                    start: b"0-0".to_vec(),
                    count: AUTOCLAIM_COUNT,
                },
                "XAUTOCLAIM wsg:x workers alive 60000 0-0 COUNT 100",
            ),
            (
                StreamAction::GroupCreate {
                    group: g.clone(),
                    start: b"$".to_vec(),
                },
                "XGROUP CREATE wsg:x workers $",
            ),
            (
                StreamAction::GroupDestroy { group: g.clone() },
                "XGROUP DESTROY wsg:x workers",
            ),
            (
                StreamAction::Trim {
                    strategy: TrimStrategy::MaxLen,
                    approximate: false,
                    threshold: b"1000".to_vec(),
                },
                "XTRIM wsg:x MAXLEN 1000",
            ),
            (
                StreamAction::Trim {
                    strategy: TrimStrategy::MinId,
                    approximate: true,
                    threshold: b"1700000000000-0".to_vec(),
                },
                "XTRIM wsg:x MINID ~ 1700000000000-0",
            ),
        ];
        for (action, expected) in cases {
            let text = action.command(k);
            assert_eq!(text, expected);
            // One script line, reviewed as a data change, never a read.
            let commands = parse_script(&text).unwrap();
            assert_eq!(commands.len(), 1, "{text}");
            assert!(
                crate::mutation_review::analyze_redis(&text).is_some(),
                "{text} must be reviewed as a write"
            );
        }
    }

    #[test]
    fn redis_streams_trim_and_destroy_require_the_typed_text() {
        let trim = StreamAction::Trim {
            strategy: TrimStrategy::MaxLen,
            approximate: false,
            threshold: b"1000".to_vec(),
        };
        let expected = trim.typed_confirmation().unwrap();
        assert_eq!(expected, "1000");
        assert!(!typed_confirmation_matches(&expected, ""));
        assert!(!typed_confirmation_matches(&expected, "100"));
        assert!(!typed_confirmation_matches(&expected, "1,000"));
        assert!(typed_confirmation_matches(&expected, " 1000 "));

        let destroy = StreamAction::GroupDestroy {
            group: b"workers".to_vec(),
        };
        let expected = destroy.typed_confirmation().unwrap();
        assert_eq!(expected, "workers");
        assert!(!typed_confirmation_matches(&expected, "Workers"));
        assert!(typed_confirmation_matches(&expected, "workers"));
        // A binary group name is confirmed by its hex, as the editor shows it.
        let binary = StreamAction::GroupDestroy {
            group: vec![0xff, 0x00],
        };
        assert_eq!(binary.typed_confirmation().unwrap(), "ff 00");
        assert!(!typed_confirmation_matches("", ""));

        for action in [
            StreamAction::Delete {
                id: b"1-0".to_vec(),
            },
            StreamAction::Ack {
                group: b"g".to_vec(),
                id: b"1-0".to_vec(),
            },
            StreamAction::Append {
                id: b"*".to_vec(),
                pairs: vec![(b"f".to_vec(), b"v".to_vec())],
            },
        ] {
            assert!(action.typed_confirmation().is_none(), "{action:?}");
        }
    }

    #[test]
    fn redis_streams_validate_ids_and_counts_without_reformatting() {
        assert_eq!(digits(" 0060000 ", "idle").unwrap(), "0060000");
        assert!(digits("1e3", "count").is_err());
        assert!(digits("-1", "count").is_err());
        assert!(digits("", "count").is_err());
        assert_eq!(stream_id("*", &["*"]).unwrap(), b"*");
        assert_eq!(stream_id("$", &["$", "0"]).unwrap(), b"$");
        assert_eq!(
            stream_id("1700000000000-3", &[]).unwrap(),
            b"1700000000000-3"
        );
        assert_eq!(stream_id("1700000000000", &[]).unwrap(), b"1700000000000");
        assert_eq!(stream_id("5-*", &["*"]).unwrap(), b"5-*");
        assert!(stream_id("$", &[]).is_err());
        assert!(stream_id("abc", &["*"]).is_err());
        assert!(stream_id("1-2-3", &[]).is_err());
    }

    /// Fields and values of an appended entry reach the command byte for
    /// byte, whatever they hold.
    #[test]
    fn redis_streams_append_keeps_bytes_exact() {
        let samples: Vec<Vec<u8>> = vec![
            b"00014".to_vec(),
            Vec::new(),
            vec![0x00],
            vec![0xff, 0xfe],
            b"12345678901234567890".to_vec(),
            b"quote\"back\\slash".to_vec(),
            b"line\nbreak".to_vec(),
            "héllo ✓".as_bytes().to_vec(),
        ];
        for value in &samples {
            let action = StreamAction::Append {
                id: b"*".to_vec(),
                pairs: vec![(value.clone(), value.clone())],
            };
            let text = action.command(b"wsg:bytes");
            let commands = parse_script(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(commands[0].args[3], *value, "{text}");
            assert_eq!(commands[0].args[4], *value, "{text}");
            let ack = StreamAction::Claim {
                group: value.clone(),
                consumer: value.clone(),
                min_idle_ms: "0".into(),
                id: b"1-0".to_vec(),
            }
            .command(b"k");
            let commands = parse_script(&ack).unwrap();
            assert_eq!(commands[0].args[2], *value);
            assert_eq!(commands[0].args[3], *value);
        }
    }
}
