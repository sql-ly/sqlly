//! Persisted AI conversations. Text and SQL go through `redact_sql` before disk.

use crate::host_storage::{StorageArea, load_text, remove, store_text};
use crate::intellisense_feedback::redact_sql;
use std::path::Path;

pub const CONVERSATIONS_DIR: &str = "ai-conversations";
pub const INDEX_FILE: &str = "ai-conversations/index.json";
pub const MAX_CONVERSATIONS: usize = 50;
const TITLE_CHARS: usize = 60;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredMessage {
    pub role: String,
    pub text: String,
    #[serde(default)]
    pub sql: Option<String>,
    #[serde(default)]
    pub is_error: bool,
    /// `"text"` (default), `"tool_call"`, `"tool_result"`, or `"note"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_chars: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub lane: String,
    pub server_id: String,
    pub database: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub pinned_context: Vec<String>,
    pub messages: Vec<StoredMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub lane: String,
    pub server_id: String,
    pub database: Option<String>,
    pub updated_at: String,
    pub message_count: usize,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ConversationIndex {
    pub entries: Vec<ConversationSummary>,
}

/// Non-gpui view of a chat turn so this module never imports `ai_panel`.
#[derive(Clone, Debug)]
pub struct ChatMessageView {
    pub role: String,
    pub kind: String,
    pub text: String,
    pub sql: Option<String>,
    pub is_pending: bool,
    pub is_error: bool,
    pub tool_name: Option<String>,
    pub tool_chars: Option<usize>,
}

/// Redacting projection: `text` and `sql` both pass through `redact_sql`;
/// pending messages are dropped.
pub fn project(messages: &[ChatMessageView]) -> Vec<StoredMessage> {
    messages
        .iter()
        .filter(|message| !message.is_pending)
        .map(|message| StoredMessage {
            role: message.role.clone(),
            text: redact_sql(&message.text),
            sql: message.sql.as_deref().map(redact_sql),
            is_error: message.is_error,
            kind: if message.kind == "text" {
                String::new()
            } else {
                message.kind.clone()
            },
            tool_name: message.tool_name.clone(),
            tool_chars: message.tool_chars,
        })
        .collect()
}

pub fn title_from_first_user_message(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    let mut title: String = line.chars().take(TITLE_CHARS).collect();
    if line.chars().count() > TITLE_CHARS {
        title.push('…');
    }
    if title.is_empty() {
        crate::localization::text("ai.conversations.new_chat_title")
    } else {
        title
    }
}

pub fn now_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// One-line subtitle for a history row: `Generate · 3h ago`.
pub fn history_meta_line(
    lane: &str,
    updated_at: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let label = match lane {
        "generate" => crate::localization::text("ai.conversations.lane_generate"),
        "ask" => crate::localization::text("ai.conversations.lane_ask"),
        "search" => crate::localization::text("ai.conversations.lane_search"),
        "joins" => crate::localization::text("ai.conversations.lane_joins"),
        other => other.to_string(),
    };
    let Some(updated) = chrono::DateTime::parse_from_rfc3339(updated_at)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
    else {
        return crate::localization::format(
            "ai.conversations.history_timestamp",
            &[("lane", &label), ("updated_at", &updated_at.to_string())],
        );
    };
    crate::localization::format(
        "ai.conversations.history_relative",
        &[
            ("lane", &label),
            (
                "age",
                &history_relative_age(now.signed_duration_since(updated)),
            ),
        ],
    )
}

fn history_relative_age(age: chrono::Duration) -> String {
    let seconds = age.num_seconds().max(0);
    let key = if seconds < 60 {
        "ai.conversations.age_just_now"
    } else if seconds < 3_600 {
        "ai.conversations.age_minutes"
    } else if seconds < 86_400 {
        "ai.conversations.age_hours"
    } else {
        "ai.conversations.age_days"
    };
    if seconds < 60 {
        crate::localization::text(key)
    } else {
        let count = if seconds < 3_600 {
            seconds / 60
        } else if seconds < 86_400 {
            seconds / 3_600
        } else {
            seconds / 86_400
        }
        .to_string();
        crate::localization::format(key, &[("count", &count)])
    }
}

pub fn new_conversation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn serialize_conversation(conversation: &Conversation) -> Result<String, String> {
    serde_json::to_string_pretty(conversation).map_err(|e| e.to_string())
}

pub fn load_index() -> ConversationIndex {
    load_text(StorageArea::Data, INDEX_FILE)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn load_conversation(id: &str) -> Option<Conversation> {
    load_text(StorageArea::Data, &conversation_file(id))
        .and_then(|text| serde_json::from_str(&text).ok())
}

pub fn save_conversation(conversation: &Conversation) -> Result<(), String> {
    save_conversation_with(
        conversation,
        |name, contents| store_text(StorageArea::Data, name, contents),
        |name| load_text(StorageArea::Data, name),
        |name| remove(StorageArea::Data, name),
    )
}

pub fn delete_conversation(id: &str) {
    remove(StorageArea::Data, &conversation_file(id));
    let mut index = load_index();
    index.entries.retain(|entry| entry.id != id);
    let _ = write_index(&index, |name, contents| {
        store_text(StorageArea::Data, name, contents)
    });
}

pub fn search_index(index: &ConversationIndex, query: &str) -> Vec<ConversationSummary> {
    if query.trim().is_empty() {
        let mut entries = index.entries.clone();
        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        return entries;
    }
    let query_lower = query.to_lowercase();
    let mut scored: Vec<(i32, ConversationSummary)> = index
        .entries
        .iter()
        .filter_map(|entry| {
            crate::command_palette::CommandPaletteMatcher::subsequence_score(
                &query_lower,
                &entry.title.to_lowercase(),
            )
            .map(|score| (score, entry.clone()))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.updated_at.cmp(&a.1.updated_at))
    });
    scored.into_iter().map(|(_, entry)| entry).collect()
}

pub fn export_markdown(conversation: &Conversation) -> String {
    let db = conversation.database.as_deref().unwrap_or("—");
    let header = crate::localization::format(
        "ai.conversations.export_header",
        &[
            ("title", &conversation.title),
            ("server", &conversation.server_id),
            ("database", &db.to_string()),
            ("lane", &conversation.lane),
            ("updated_at", &conversation.updated_at),
        ],
    );
    let mut out = header;
    if !conversation.pinned_context.is_empty() {
        let pinned = crate::localization::format(
            "ai.conversations.export_pinned_context",
            &[("context", &conversation.pinned_context.join(", "))],
        );
        out.push_str(&pinned);
    }
    out.push('\n');
    for message in &conversation.messages {
        let heading = match message.kind.as_str() {
            "tool_call" | "tool_result" => {
                crate::localization::text("ai.conversations.export_tool_heading")
            }
            _ if message.role == "user" => {
                crate::localization::text("ai.conversations.export_user_heading")
            }
            _ => crate::localization::text("ai.conversations.export_assistant_heading"),
        };
        out.push_str(&format!("## {heading}\n\n"));
        if !message.text.is_empty() {
            out.push_str(&message.text);
            out.push_str("\n\n");
        }
        if let Some(sql) = &message.sql {
            out.push_str("```sql\n");
            out.push_str(sql);
            out.push_str("\n```\n\n");
        }
    }
    out
}

fn conversation_file(id: &str) -> String {
    format!("{CONVERSATIONS_DIR}/{id}.json")
}

fn save_conversation_with(
    conversation: &Conversation,
    store: impl Fn(&str, &str) -> Result<(), String>,
    load: impl Fn(&str) -> Option<String>,
    remove_file: impl Fn(&str),
) -> Result<(), String> {
    let json = serialize_conversation(conversation)?;
    store(&conversation_file(&conversation.id), &json)?;
    let mut index: ConversationIndex = load(INDEX_FILE)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let summary = ConversationSummary {
        id: conversation.id.clone(),
        title: conversation.title.clone(),
        lane: conversation.lane.clone(),
        server_id: conversation.server_id.clone(),
        database: conversation.database.clone(),
        updated_at: conversation.updated_at.clone(),
        message_count: conversation.messages.len(),
    };
    if let Some(existing) = index
        .entries
        .iter_mut()
        .find(|entry| entry.id == summary.id)
    {
        *existing = summary;
    } else {
        index.entries.push(summary);
    }
    index
        .entries
        .sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    while index.entries.len() > MAX_CONVERSATIONS {
        let evicted = index.entries.pop().expect("len > max");
        remove_file(&conversation_file(&evicted.id));
    }
    write_index(&index, store)
}

fn write_index(
    index: &ConversationIndex,
    store: impl Fn(&str, &str) -> Result<(), String>,
) -> Result<(), String> {
    let json = serde_json::to_string_pretty(index).map_err(|e| e.to_string())?;
    store(INDEX_FILE, &json)
}

/// Path-parameterized save for tests (mirrors `query_history::save_pins_to`).
// Retained for future use.
#[allow(dead_code)]
#[cfg(not(target_arch = "wasm32"))]
pub fn save_conversation_to(root: &Path, conversation: &Conversation) -> Result<(), String> {
    save_conversation_with(
        conversation,
        |name, contents| {
            let path = root.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(path, contents).map_err(|e| e.to_string())
        },
        |name| std::fs::read_to_string(root.join(name)).ok(),
        |name| {
            let _ = std::fs::remove_file(root.join(name));
        },
    )
}

// Retained for future use.
#[allow(dead_code)]
#[cfg(not(target_arch = "wasm32"))]
pub fn load_index_from(root: &Path) -> ConversationIndex {
    std::fs::read_to_string(root.join(INDEX_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(
        role: &str,
        text: &str,
        sql: Option<&str>,
        pending: bool,
        error: bool,
    ) -> ChatMessageView {
        ChatMessageView {
            role: role.into(),
            kind: "text".into(),
            text: text.into(),
            sql: sql.map(str::to_string),
            is_pending: pending,
            is_error: error,
            tool_name: None,
            tool_chars: None,
        }
    }

    #[test]
    fn projection_redacts_literals_in_text_and_sql() {
        let stored = project(&[view(
            "user",
            "password is 'hunter2'",
            Some("SELECT 0xDEADBEEF, 'hunter2'"),
            false,
            false,
        )]);
        assert_eq!(stored.len(), 1);
        assert!(!stored[0].text.contains("hunter2"));
        assert!(stored[0].text.contains("'<redacted>'"));
        let sql = stored[0].sql.as_deref().unwrap();
        assert!(!sql.contains("hunter2"));
        assert!(!sql.contains("DEADBEEF"));
        assert!(sql.contains("'<redacted>'"));
        assert!(sql.contains("0x0"));
    }

    #[test]
    fn persisted_conversation_never_contains_a_raw_literal() {
        let messages = project(&[
            view(
                "user",
                "login 'hunter2' blob 0xDEADBEEF",
                Some("PASSWORD = 'hunter2', 0xDEADBEEF"),
                false,
                false,
            ),
            view(
                "assistant",
                "got 'hunter2'",
                Some("SELECT 'hunter2', 0xDEADBEEF"),
                false,
                false,
            ),
        ]);
        let conversation = Conversation {
            id: "tripwire".into(),
            title: title_from_first_user_message(&messages[0].text),
            lane: "ask".into(),
            server_id: "s".into(),
            database: Some("db".into()),
            created_at: "2026-08-28T00:00:00Z".into(),
            updated_at: "2026-08-28T00:00:00Z".into(),
            pinned_context: vec![],
            messages,
        };
        let json = serialize_conversation(&conversation).unwrap();
        assert!(!json.contains("hunter2"), "raw literal leaked: {json}");
        assert!(!json.contains("DEADBEEF"), "hex blob leaked: {json}");
        assert!(json.contains("'<redacted>'"));
    }

    #[test]
    fn pending_messages_are_dropped() {
        let stored = project(&[
            view("user", "hi", None, false, false),
            view("assistant", "", None, true, false),
        ]);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].role, "user");
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn lru_evicts_oldest_beyond_50() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_CONVERSATIONS + 3) {
            let conversation = Conversation {
                id: format!("id-{i:03}"),
                title: format!("Chat {i}"),
                lane: "ask".into(),
                server_id: "s".into(),
                database: None,
                created_at: format!("2026-08-28T00:{i:02}:00Z"),
                updated_at: format!("2026-08-28T00:{i:02}:00Z"),
                pinned_context: vec![],
                messages: vec![StoredMessage {
                    role: "user".into(),
                    text: "hi".into(),
                    sql: None,
                    is_error: false,
                    kind: String::new(),
                    tool_name: None,
                    tool_chars: None,
                }],
            };
            save_conversation_to(dir.path(), &conversation).unwrap();
        }
        let index = load_index_from(dir.path());
        assert_eq!(index.entries.len(), MAX_CONVERSATIONS);
        assert!(index.entries.iter().all(|e| e.id != "id-000"));
        assert!(!dir.path().join("ai-conversations/id-000.json").exists());
        assert!(dir.path().join("ai-conversations/id-052.json").exists());
    }

    #[test]
    fn search_index_empty_query_is_newest_first() {
        let index = ConversationIndex {
            entries: vec![
                ConversationSummary {
                    id: "old".into(),
                    title: "Older".into(),
                    lane: "ask".into(),
                    server_id: "s".into(),
                    database: None,
                    updated_at: "2026-08-01T00:00:00Z".into(),
                    message_count: 1,
                },
                ConversationSummary {
                    id: "new".into(),
                    title: "Newer".into(),
                    lane: "ask".into(),
                    server_id: "s".into(),
                    database: None,
                    updated_at: "2026-08-28T00:00:00Z".into(),
                    message_count: 2,
                },
            ],
        };
        let ranked = search_index(&index, "");
        assert_eq!(ranked[0].id, "new");
        assert_eq!(ranked[1].id, "old");
    }

    #[test]
    fn export_markdown_matches_golden() {
        let conversation = Conversation {
            id: "g".into(),
            title: "Count orders".into(),
            lane: "ask".into(),
            server_id: "warehouse".into(),
            database: Some("sales".into()),
            created_at: "2026-08-28T10:00:00Z".into(),
            updated_at: "2026-08-28T10:01:00Z".into(),
            pinned_context: vec!["dbo.Orders".into(), "dbo.Customers".into()],
            messages: vec![
                StoredMessage {
                    role: "user".into(),
                    text: "How many orders?".into(),
                    sql: None,
                    is_error: false,
                    kind: String::new(),
                    tool_name: None,
                    tool_chars: None,
                },
                StoredMessage {
                    role: "assistant".into(),
                    text: "Use a count.".into(),
                    sql: Some("SELECT COUNT(*) FROM dbo.Orders".into()),
                    is_error: false,
                    kind: String::new(),
                    tool_name: None,
                    tool_chars: None,
                },
            ],
        };
        let golden = "# Count orders\n\nServer: warehouse · Database: sales · Lane: ask · Updated: 2026-08-28T10:01:00Z\n\nPinned context: dbo.Orders, dbo.Customers\n\n## User\n\nHow many orders?\n\n## Assistant\n\nUse a count.\n\n```sql\nSELECT COUNT(*) FROM dbo.Orders\n```\n\n";
        assert_eq!(export_markdown(&conversation), golden);
    }

    #[test]
    fn history_meta_line_uses_mode_label_and_relative_age() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-29T16:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            history_meta_line("generate", "2026-08-29T15:00:00Z", now),
            "Generate · 1h ago"
        );
        assert_eq!(
            history_meta_line("ask", "2026-08-29T15:55:00Z", now),
            "Ask · 5m ago"
        );
        assert_eq!(
            history_meta_line("search", "2026-08-29T16:00:00Z", now),
            "Search · just now"
        );
        assert_eq!(
            history_meta_line("mystery", "not-a-timestamp", now),
            "mystery · not-a-timestamp"
        );
    }
}
