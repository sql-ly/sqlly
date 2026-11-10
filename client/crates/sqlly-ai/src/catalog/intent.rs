//! SQL intent detection.
//!
//! Two complementary classifiers live here:
//!
//! - [`detect_intent`] reads a natural-language request ("show me all
//!   customers", "delete the old logs") and guesses the operation the user is
//!   asking for. It is deliberately conservative — it informs generation
//!   metadata, not safety decisions.
//! - [`detect_sql_intent`] parses actual SQL and classifies it from the parsed
//!   statement nodes. This is the authoritative intent once SQL exists, and it
//!   is what safety checks key off.
//!
//! When SQL is available the parsed intent always wins; the natural-language
//! guess is only a fallback for cases the parser cannot classify (e.g. the model
//! returned prose instead of SQL).

use sqlly_lang::sqlmodel::Dialect;
use sqlly_lang::sqlparse::{StatementNode, parse};

use crate::catalog::text::contains_word;

/// The operation a SQL statement (or a request for one) represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlIntent {
    Select,
    Insert,
    Update,
    Delete,
    Merge,
    /// Data-definition (CREATE/DROP/ALTER/TRUNCATE).
    Ddl,
    /// Multiple statements with differing intents.
    Mixed,
    /// Could not be classified.
    Unknown,
}

impl SqlIntent {
    /// Whether the intent writes data or schema (anything but a read).
    pub fn is_mutating(self) -> bool {
        matches!(
            self,
            SqlIntent::Insert
                | SqlIntent::Update
                | SqlIntent::Delete
                | SqlIntent::Merge
                | SqlIntent::Ddl
                | SqlIntent::Mixed
        )
    }

    /// Whether the intent can irreversibly change or remove data/schema.
    pub fn is_destructive(self) -> bool {
        matches!(
            self,
            SqlIntent::Update | SqlIntent::Delete | SqlIntent::Ddl | SqlIntent::Merge
        )
    }

    /// Stable lowercase token for logging/serialization-adjacent uses.
    pub fn as_str(self) -> &'static str {
        match self {
            SqlIntent::Select => "select",
            SqlIntent::Insert => "insert",
            SqlIntent::Update => "update",
            SqlIntent::Delete => "delete",
            SqlIntent::Merge => "merge",
            SqlIntent::Ddl => "ddl",
            SqlIntent::Mixed => "mixed",
            SqlIntent::Unknown => "unknown",
        }
    }
}

/// Classify a natural-language request into a likely SQL intent.
///
/// Conservative keyword matching: destructive/mutating verbs are checked before
/// read verbs so "delete customers that have no orders" classifies as `Delete`
/// rather than being swayed by the noun. Returns [`SqlIntent::Unknown`] when no
/// recognizable verb is present.
pub fn detect_intent(prompt: &str) -> SqlIntent {
    let lower = prompt.to_lowercase();
    let has = |needle: &str| contains_word(&lower, needle);

    // DDL first — these words are unambiguous and the most dangerous to miss.
    if has("drop")
        || has("truncate")
        || has("alter")
        || phrase(&lower, &["create", "table"])
        || phrase(&lower, &["create", "view"])
        || phrase(&lower, &["create", "index"])
    {
        return SqlIntent::Ddl;
    }
    if has("merge") || phrase(&lower, &["upsert"]) {
        return SqlIntent::Merge;
    }
    if has("delete") || has("remove") || has("purge") {
        return SqlIntent::Delete;
    }
    if has("update") || has("modify") || phrase(&lower, &["set"]) || has("change") {
        return SqlIntent::Update;
    }
    if has("insert") || phrase(&lower, &["add"]) || has("create") {
        return SqlIntent::Insert;
    }
    if has("select")
        || has("show")
        || has("list")
        || has("find")
        || has("count")
        || has("get")
        || has("fetch")
        || has("query")
        || has("report")
        || has("which")
        || has("what")
        || has("how")
    {
        return SqlIntent::Select;
    }
    SqlIntent::Unknown
}

/// Classify the intent of one or more SQL statements by parsing them.
///
/// If all statements share an intent, that intent is returned; differing
/// statement intents yield [`SqlIntent::Mixed`]. Empty input is
/// [`SqlIntent::Unknown`].
pub fn detect_sql_intent(sql: &str, dialect: Dialect) -> SqlIntent {
    let tree = parse(sql, dialect);
    intent_from_statements(&tree.statements)
}

/// Reduce a slice of parsed statements to a single intent.
pub(crate) fn intent_from_statements(statements: &[StatementNode]) -> SqlIntent {
    let mut current: Option<SqlIntent> = None;
    for stmt in statements {
        let intent = statement_intent(stmt);
        match current {
            None => current = Some(intent),
            Some(existing) if existing == intent => {}
            Some(_) => return SqlIntent::Mixed,
        }
    }
    current.unwrap_or(SqlIntent::Unknown)
}

/// Map a single parsed statement to its intent.
pub(crate) fn statement_intent(stmt: &StatementNode) -> SqlIntent {
    match stmt {
        StatementNode::Select(_) => SqlIntent::Select,
        StatementNode::Insert(_) => SqlIntent::Insert,
        StatementNode::Update(_) => SqlIntent::Update,
        StatementNode::Delete(_) => SqlIntent::Delete,
        StatementNode::CreateTable(_)
        | StatementNode::CreateView(_)
        | StatementNode::CreateRoutine(_) => SqlIntent::Ddl,
        StatementNode::Exec(_) => SqlIntent::Unknown,
        StatementNode::Opaque { head, .. } => intent_from_opaque_head(head),
    }
}

/// Classify an [`StatementNode::Opaque`] head (a lowercased leading keyword
/// phrase such as "drop table", "truncate", "alter table", "merge").
fn intent_from_opaque_head(head: &str) -> SqlIntent {
    let first = head.split_whitespace().next().unwrap_or("");
    match first {
        "drop" | "truncate" | "alter" | "create" => SqlIntent::Ddl,
        "merge" => SqlIntent::Merge,
        _ => SqlIntent::Unknown,
    }
}

/// All listed words appear (each as a whole word) somewhere in the haystack.
fn phrase(haystack: &str, words: &[&str]) -> bool {
    words.iter().all(|w| contains_word(haystack, w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_language_read_verbs_are_select() {
        for q in [
            "show me all customers",
            "list the orders from last week",
            "how many users signed up",
            "count active sessions",
            "find customers without orders",
            "what is the average order total",
        ] {
            assert_eq!(detect_intent(q), SqlIntent::Select, "{q}");
        }
    }

    #[test]
    fn natural_language_delete_verbs() {
        assert_eq!(detect_intent("delete old logs"), SqlIntent::Delete);
        assert_eq!(detect_intent("remove cancelled orders"), SqlIntent::Delete);
        assert_eq!(detect_intent("purge expired sessions"), SqlIntent::Delete);
    }

    #[test]
    fn natural_language_update_verbs() {
        assert_eq!(
            detect_intent("update the customer email"),
            SqlIntent::Update
        );
        assert_eq!(
            detect_intent("change the status to active"),
            SqlIntent::Update
        );
        assert_eq!(detect_intent("set the price to 10"), SqlIntent::Update);
    }

    #[test]
    fn natural_language_ddl_verbs() {
        assert_eq!(detect_intent("drop the temp table"), SqlIntent::Ddl);
        assert_eq!(detect_intent("create table widgets"), SqlIntent::Ddl);
        assert_eq!(detect_intent("truncate the audit log"), SqlIntent::Ddl);
        assert_eq!(detect_intent("alter the orders table"), SqlIntent::Ddl);
    }

    #[test]
    fn natural_language_unknown_when_no_verb() {
        assert_eq!(detect_intent("the quick brown fox"), SqlIntent::Unknown);
    }

    #[test]
    fn whole_word_matching_avoids_false_positives() {
        // "asset" must not match "set"; "address" must not match "add".
        assert_eq!(detect_intent("the asset register"), SqlIntent::Unknown);
        assert_eq!(detect_intent("the address book"), SqlIntent::Unknown);
    }

    #[test]
    fn sql_intent_select() {
        assert_eq!(
            detect_sql_intent("SELECT id FROM dbo.Customers", Dialect::SqlServer),
            SqlIntent::Select
        );
    }

    #[test]
    fn sql_intent_update_delete_insert() {
        assert_eq!(
            detect_sql_intent("UPDATE dbo.t SET x = 1 WHERE id = 2", Dialect::SqlServer),
            SqlIntent::Update
        );
        assert_eq!(
            detect_sql_intent("DELETE FROM dbo.t WHERE id = 2", Dialect::SqlServer),
            SqlIntent::Delete
        );
        assert_eq!(
            detect_sql_intent("INSERT INTO dbo.t (x) VALUES (1)", Dialect::SqlServer),
            SqlIntent::Insert
        );
    }

    #[test]
    fn sql_intent_ddl_via_opaque_head() {
        assert_eq!(
            detect_sql_intent("DROP TABLE dbo.t", Dialect::SqlServer),
            SqlIntent::Ddl
        );
        assert_eq!(
            detect_sql_intent("TRUNCATE TABLE dbo.t", Dialect::SqlServer),
            SqlIntent::Ddl
        );
        assert_eq!(
            detect_sql_intent("ALTER TABLE dbo.t ADD x INT", Dialect::SqlServer),
            SqlIntent::Ddl
        );
    }

    #[test]
    fn sql_intent_create_table_is_ddl() {
        assert_eq!(
            detect_sql_intent("CREATE TABLE dbo.t (id INT)", Dialect::SqlServer),
            SqlIntent::Ddl
        );
    }

    #[test]
    fn sql_intent_mixed_for_differing_statements() {
        let sql = "SELECT 1 FROM dbo.t; DELETE FROM dbo.t WHERE id = 1";
        assert_eq!(detect_sql_intent(sql, Dialect::SqlServer), SqlIntent::Mixed);
    }

    #[test]
    fn sql_intent_unknown_for_empty() {
        assert_eq!(
            detect_sql_intent("", Dialect::SqlServer),
            SqlIntent::Unknown
        );
    }

    #[test]
    fn intent_classifications_have_consistent_flags() {
        assert!(SqlIntent::Delete.is_mutating());
        assert!(SqlIntent::Delete.is_destructive());
        assert!(SqlIntent::Ddl.is_destructive());
        assert!(!SqlIntent::Select.is_mutating());
        assert!(!SqlIntent::Select.is_destructive());
        assert!(SqlIntent::Insert.is_mutating());
        assert!(!SqlIntent::Insert.is_destructive());
    }
}
