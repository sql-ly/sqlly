//! Per-engine description (comment) DDL for tables, views, and columns.
//!
//! One staged [`CommentChange`] renders in the engine's own form:
//! - **SQL Server / Azure SQL**: `sp_addextendedproperty` when the object had
//!   no `MS_Description`, `sp_updateextendedproperty` when it did, and
//!   `sp_dropextendedproperty` when the description is cleared.
//! - **PostgreSQL / DuckDB**: `COMMENT ON TABLE|VIEW|COLUMN … IS '…'` (`IS NULL`
//!   clears).
//! - **Oracle**: `COMMENT ON TABLE|COLUMN … IS '…'` (Oracle addresses views
//!   with the TABLE keyword; `IS ''` clears because Oracle stores empty as NULL).
//! - **MySQL / MariaDB**: table descriptions via `ALTER TABLE … COMMENT = '…'`;
//!   column descriptions via `ALTER TABLE … MODIFY COLUMN … COMMENT '…'` where
//!   the definition is restated from the engine's own `SHOW CREATE TABLE`
//!   text (only the COMMENT clause changes), and the edit is refused when
//!   that text is unavailable — a rebuild from the typed snapshot subset
//!   could silently drop generated expressions, charset/collation, ON
//!   UPDATE, or INVISIBLE. MySQL generated stored columns use the documented
//!   metadata-only INPLACE operation. MariaDB generated persistent columns
//!   require ALGORITHM=INSTANT so a comment edit cannot silently regenerate
//!   their values. Views carry no comment on MySQL.
//! - **ClickHouse**: `ALTER TABLE … MODIFY COMMENT '…'` and
//!   `ALTER TABLE … COMMENT COLUMN … '…'` — ClickHouse has no `COMMENT ON`.
//! - **SQLite / libSQL / Redis**: no server-side comment store; the callers
//!   hide the field and these engines return an honest error here.

use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType, Dialect};

use crate::alter::script::qualified_table;

/// The kind of object a description targets. Only tables and views carry
/// descriptions in the editors today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentObjectKind {
    Table,
    View,
}

/// One staged description change. `column: None` addresses the object itself;
/// `from` is the live server value (`None` = never set) and `to` the desired
/// value (`None` = clear it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentChange {
    pub column: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// Whether the engine stores object/column descriptions on the server at all.
pub fn supports_comments(db: DatabaseType) -> bool {
    matches!(
        db.connection_kind(),
        ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::DuckDbFile
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::OracleNet
    )
}

/// The plain-language reason the description field is unavailable, shown
/// where the editors hide it (Engine-Aware Action Availability). `None` when
/// the engine supports comments.
pub fn unsupported_reason_message(db: DatabaseType) -> Option<sqlly_core::DisplayMessage> {
    if supports_comments(db) {
        return None;
    }
    use sqlly_core::DisplayMessage;
    let message = match db.connection_kind() {
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => DisplayMessage::new(
            "services.alter.comments.no_server_comments",
            format!("{} has no server-side comments", db.display_name()),
        )
        .with_arg("engine_name", db.display_name()),
        ConnectionKind::Resp => DisplayMessage::new(
            "services.alter.comments.redis_unsupported",
            "Redis is a key-value store with no object comments",
        ),
        _ => DisplayMessage::new(
            "services.alter.comments.engine_unsupported",
            format!(
                "Object descriptions are not supported for {} connections",
                db.display_name()
            ),
        )
        .with_arg("engine_name", db.display_name()),
    };
    Some(message)
}

pub fn unsupported_reason(db: DatabaseType) -> Option<String> {
    unsupported_reason_message(db).map(sqlly_core::DisplayMessage::into_fallback)
}

/// The plain-language reason descriptions are unavailable for this kind of
/// object on this engine: [`unsupported_reason`] plus the engines that store
/// table descriptions but not view descriptions (MySQL/MariaDB report every
/// view's comment as the literal `VIEW`; ClickHouse has no view `ALTER …
/// COMMENT`; CockroachDB has no `COMMENT ON VIEW`). `None` when supported.
pub fn unsupported_reason_for_message(
    db: DatabaseType,
    kind: CommentObjectKind,
) -> Option<sqlly_core::DisplayMessage> {
    if let Some(reason) = unsupported_reason_message(db) {
        return Some(reason);
    }
    if kind != CommentObjectKind::View {
        return None;
    }
    use sqlly_core::DisplayMessage;
    match db.connection_kind() {
        ConnectionKind::MySqlWire => Some(
            DisplayMessage::new(
                "services.alter.comments.mysql_view_unsupported",
                format!(
                    "{} cannot store a description on a view (its comment is always 'VIEW')",
                    db.display_name()
                ),
            )
            .with_arg("engine_name", db.display_name()),
        ),
        ConnectionKind::ClickHouseHttp => Some(DisplayMessage::new(
            "services.alter.comments.clickhouse_view_unsupported",
            "ClickHouse does not support ALTER … COMMENT on views; recreate the view with a COMMENT clause",
        )),
        _ if db == DatabaseType::CockroachDb => Some(DisplayMessage::new(
            "services.alter.comments.cockroach_view_unsupported",
            "CockroachDB has no COMMENT ON VIEW; view descriptions cannot be stored",
        )),
        _ => None,
    }
}

pub fn unsupported_reason_for(db: DatabaseType, kind: CommentObjectKind) -> Option<String> {
    unsupported_reason_for_message(db, kind).map(sqlly_core::DisplayMessage::into_fallback)
}

/// Render one change in the engine's own form. `source_ddl` carries the
/// engine's own `CREATE TABLE` text for the MySQL `MODIFY COLUMN` path —
/// required there (the definition is restated from it verbatim), ignored
/// elsewhere. Returns `Ok(None)` when there is nothing to do.
pub fn render_comment_change(
    db: DatabaseType,
    schema: Option<&str>,
    object: &str,
    kind: CommentObjectKind,
    change: &CommentChange,
    source_ddl: Option<&str>,
) -> Result<Option<String>, String> {
    render_comment_change_detailed(db, schema, object, kind, change, source_ddl)
        .map_err(sqlly_core::DisplayMessage::into_fallback)
}

pub fn render_comment_change_detailed(
    db: DatabaseType,
    schema: Option<&str>,
    object: &str,
    kind: CommentObjectKind,
    change: &CommentChange,
    source_ddl: Option<&str>,
) -> Result<Option<String>, sqlly_core::DisplayMessage> {
    if change.from == change.to {
        return Ok(None);
    }
    if let Some(reason) = unsupported_reason_for_message(db, kind) {
        return Err(reason);
    }
    if db.connection_kind() == ConnectionKind::ClickHouseHttp {
        return render_clickhouse(db, schema, object, change)
            .map_err(sqlly_core::DisplayMessage::raw);
    }
    if db == DatabaseType::DuckDb {
        return render_comment_on(db, schema, object, kind, change, "NULL")
            .map_err(sqlly_core::DisplayMessage::raw);
    }
    match db.family() {
        Dialect::SqlServer => Ok(render_extended_property(schema, object, kind, change)),
        Dialect::Postgres => render_comment_on(db, schema, object, kind, change, "NULL")
            .map_err(sqlly_core::DisplayMessage::raw),
        Dialect::MySql => render_mysql(db, schema, object, change, source_ddl),
        Dialect::Oracle => render_comment_on(db, schema, object, kind, change, "''")
            .map_err(sqlly_core::DisplayMessage::raw),
        Dialect::Sqlite => Err(sqlly_core::DisplayMessage::new(
            "services.alter.comments.no_server_comments",
            format!("{} has no server-side comments", db.display_name()),
        )
        .with_arg("engine_name", db.display_name())),
    }
}

/// `'…'` with embedded quotes doubled; MySQL and ClickHouse also interpret
/// backslash escapes inside string literals, so those are doubled too.
pub(crate) fn string_literal(db: DatabaseType, text: &str) -> String {
    let mut out = text.replace('\'', "''");
    if matches!(
        db.connection_kind(),
        ConnectionKind::MySqlWire | ConnectionKind::ClickHouseHttp
    ) {
        out = out.replace('\\', "\\\\");
    }
    format!("'{out}'")
}

/// `N'…'` for SQL Server's Unicode procedure parameters.
fn ntext_literal(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

fn render_extended_property(
    schema: Option<&str>,
    object: &str,
    kind: CommentObjectKind,
    change: &CommentChange,
) -> Option<String> {
    let schema = schema.filter(|s| !s.is_empty()).unwrap_or("dbo");
    let level1 = match kind {
        CommentObjectKind::Table => "TABLE",
        CommentObjectKind::View => "VIEW",
    };
    let mut levels = format!(
        "@level0type = N'SCHEMA', @level0name = {}, @level1type = N'{level1}', @level1name = {}",
        ntext_literal(schema),
        ntext_literal(object)
    );
    if let Some(column) = &change.column {
        levels.push_str(&format!(
            ", @level2type = N'COLUMN', @level2name = {}",
            ntext_literal(column)
        ));
    }
    let proc = match (&change.from, &change.to) {
        (Some(_), None) => "sp_dropextendedproperty",
        (Some(_), Some(_)) => "sp_updateextendedproperty",
        (None, Some(_)) | (None, None) => "sp_addextendedproperty",
    };
    let value = match &change.to {
        Some(text) => format!("@value = {}, ", ntext_literal(text)),
        None => String::new(),
    };
    Some(format!(
        "EXEC {proc} @name = N'MS_Description', {value}{levels};\n"
    ))
}

/// `COMMENT ON …` for PostgreSQL, DuckDB, and Oracle. `clear` is the clause
/// value that removes a comment: `NULL` on PostgreSQL/DuckDB, `''` on Oracle.
fn render_comment_on(
    db: DatabaseType,
    schema: Option<&str>,
    object: &str,
    kind: CommentObjectKind,
    change: &CommentChange,
    clear: &str,
) -> Result<Option<String>, String> {
    let object_q = qualified_table(db, schema, object);
    let target = match &change.column {
        Some(column) => format!("COLUMN {object_q}.{}", db.quote_identifier(column)),
        None => {
            let keyword = match kind {
                // Oracle addresses views with TABLE; PostgreSQL and DuckDB have
                // a dedicated VIEW keyword.
                CommentObjectKind::Table => "TABLE",
                CommentObjectKind::View
                    if db.family() == Dialect::Postgres || db == DatabaseType::DuckDb =>
                {
                    "VIEW"
                }
                CommentObjectKind::View => "TABLE",
            };
            format!("{keyword} {object_q}")
        }
    };
    let value = match &change.to {
        Some(text) => string_literal(db, text),
        None => clear.to_string(),
    };
    Ok(Some(format!("COMMENT ON {target} IS {value};\n")))
}

fn render_mysql(
    db: DatabaseType,
    schema: Option<&str>,
    object: &str,
    change: &CommentChange,
    source_ddl: Option<&str>,
) -> Result<Option<String>, sqlly_core::DisplayMessage> {
    let table_q = qualified_table(db, schema, object);
    match &change.column {
        None => {
            let value = string_literal(db, change.to.as_deref().unwrap_or(""));
            Ok(Some(format!("ALTER TABLE {table_q} COMMENT = {value};\n")))
        }
        Some(column) => {
            // MODIFY COLUMN restates the whole definition, so it must restate
            // the engine's own text — a rebuild from the snapshot's typed
            // subset would silently drop attributes it cannot express
            // (generated expressions, charset/collation, ON UPDATE,
            // INVISIBLE). No DDL, no edit; the user runs the ALTER by hand.
            // (A column the same edit renames, retypes, or adds never gets
            // here: the structure editor folds its COMMENT into that clause.)
            let Some(ddl) = source_ddl else {
                return Err(sqlly_core::DisplayMessage::new(
                    "services.alter.comments.mysql_definition_unreadable",
                    format!("Column {column}: the table's definition could not be read, so the description change would risk dropping column attributes. Set it with ALTER TABLE … MODIFY COLUMN … COMMENT '…' instead."),
                )
                .with_arg("column_name", column));
            };
            let Some(line) = mysql_column_definition_line(ddl, column) else {
                return Err(sqlly_core::DisplayMessage::new(
                    "services.alter.comments.mysql_column_not_found",
                    format!("Column {column} was not found in the table's definition; the description change was not scripted."),
                )
                .with_arg("column_name", column));
            };
            let Some(edited) = with_mysql_column_comment(&line, change.to.as_deref(), db) else {
                return Ok(None);
            };
            let algorithm = if mysql_stored_generated_definition(&line) {
                if db == DatabaseType::MySql {
                    ", ALGORITHM=INPLACE"
                } else {
                    ", ALGORITHM=INSTANT"
                }
            } else {
                ""
            };
            Ok(Some(format!(
                "ALTER TABLE {table_q} MODIFY COLUMN {edited}{algorithm};\n"
            )))
        }
    }
}

fn mysql_stored_generated_definition(line: &str) -> bool {
    mysql_top_level_word(line, "AS").is_some()
        && (mysql_top_level_word(line, "STORED").is_some()
            || mysql_top_level_word(line, "PERSISTENT").is_some())
}

/// The one column-definition line of a `SHOW CREATE TABLE` body, without the
/// leading indent and trailing comma. MySQL and MariaDB quote every
/// identifier in this output, so the name match is exact (`` `name` ``).
pub(crate) fn mysql_column_definition_line(ddl: &str, column: &str) -> Option<String> {
    let needle = format!("`{}` ", column.replace('`', "``"));
    ddl.lines()
        .map(str::trim_start)
        .find(|line| line.starts_with(&needle))
        .map(|line| line.trim_end().trim_end_matches(',').trim_end().to_string())
}

/// Byte offset of the first top-level occurrence of the keyword `word` in a
/// `SHOW CREATE TABLE` column line: outside `'…'` strings and `` `…` `` /
/// `"…"` quoted names, at parenthesis depth 0, preceded by a space, and not
/// followed by an identifier character. Quote tracking is what keeps a
/// column named `` `it's` `` or a default of `'COMMENT x'` from fooling it.
pub(crate) fn mysql_top_level_word(line: &str, word: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' && q != b'`' {
                    i += 2;
                    continue;
                }
                if b == q {
                    // A doubled quote closes and immediately reopens.
                    quote = None;
                }
            }
            None => match b {
                b'\'' | b'`' | b'"' => quote = Some(b),
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ if depth == 0 && i > 0 && bytes[i - 1] == b' ' && line[i..].starts_with(word) => {
                    let after = bytes.get(i + word.len());
                    if !matches!(after, Some(c) if c.is_ascii_alphanumeric() || *c == b'_') {
                        return Some(i);
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// End (exclusive) of the quoted string literal opening at `start`, honouring
/// doubled quotes and backslash escapes.
fn string_literal_end(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let q = *bytes.get(start)?;
    if q != b'\'' && q != b'"' {
        return None;
    }
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == q => {
                if bytes.get(i + 1) == Some(&q) {
                    i += 2;
                } else {
                    return Some(i + 1);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// The raw `COMMENT '…'` clause of a `SHOW CREATE TABLE` column line, if any.
pub(crate) fn mysql_comment_clause(line: &str) -> Option<&str> {
    mysql_comment_range(line).map(|(start, end)| &line[start..end])
}

fn mysql_comment_range(line: &str) -> Option<(usize, usize)> {
    let start = mysql_top_level_word(line, "COMMENT")?;
    let end = string_literal_end(line, start + "COMMENT ".len())?;
    Some((start, end))
}

/// Column attributes that must stay AFTER a `COMMENT` clause: MariaDB's
/// grammar only accepts `CHECK`/`CONSTRAINT` last, and MySQL prints the
/// engine attributes and references after the comment.
const MYSQL_AFTER_COMMENT: &[&str] = &[
    "CONSTRAINT",
    "CHECK",
    "ENGINE_ATTRIBUTE",
    "SECONDARY_ENGINE_ATTRIBUTE",
    "REFERENCES",
];

/// Swap the `COMMENT '…'` clause of one column-definition line for `to`
/// (`None` clears it). Returns `None` when nothing changes: clearing a
/// comment the line does not carry. Everything else on the line — type,
/// nullability, default, charset, ON UPDATE, generated, INVISIBLE, and any
/// clause AFTER the comment (MariaDB's `CHECK (json_valid(…))`, MySQL's
/// `ENGINE_ATTRIBUTE`) — is kept byte for byte, because the engine wrote it.
pub(crate) fn with_mysql_column_comment(
    line: &str,
    to: Option<&str>,
    db: DatabaseType,
) -> Option<String> {
    let existing = mysql_comment_range(line);
    match (existing, to) {
        (Some((start, end)), Some(text)) => Some(format!(
            "{}COMMENT {}{}",
            &line[..start],
            string_literal(db, text),
            &line[end..]
        )),
        (Some((start, end)), None) => Some(format!("{}{}", line[..start].trim_end(), &line[end..])),
        (None, Some(text)) => {
            let clause = format!(" COMMENT {}", string_literal(db, text));
            let insert_at = MYSQL_AFTER_COMMENT
                .iter()
                .filter_map(|word| mysql_top_level_word(line, word))
                .min();
            Some(match insert_at {
                Some(at) => format!("{}{clause}{}", line[..at].trim_end(), &line[at - 1..]),
                None => format!("{line}{clause}"),
            })
        }
        (None, None) => None,
    }
}

fn render_clickhouse(
    db: DatabaseType,
    schema: Option<&str>,
    object: &str,
    change: &CommentChange,
) -> Result<Option<String>, String> {
    let table_q = qualified_table(db, schema, object);
    let stmt = match &change.column {
        None => format!(
            "ALTER TABLE {table_q} MODIFY COMMENT {};\n",
            string_literal(db, change.to.as_deref().unwrap_or(""))
        ),
        Some(column) => format!(
            "ALTER TABLE {table_q} COMMENT COLUMN {} {};\n",
            db.quote_identifier(column),
            string_literal(db, change.to.as_deref().unwrap_or(""))
        ),
    };
    Ok(Some(stmt))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(column: Option<&str>, from: Option<&str>, to: Option<&str>) -> CommentChange {
        CommentChange {
            column: column.map(str::to_string),
            from: from.map(str::to_string),
            to: to.map(str::to_string),
        }
    }

    #[test]
    fn support_matrix_matches_engines_with_a_comment_store() {
        assert!(supports_comments(DatabaseType::SqlServer));
        assert!(supports_comments(DatabaseType::AzureSqlDatabase));
        assert!(supports_comments(DatabaseType::Postgres));
        assert!(supports_comments(DatabaseType::CockroachDb));
        assert!(supports_comments(DatabaseType::MySql));
        assert!(supports_comments(DatabaseType::MariaDb));
        assert!(supports_comments(DatabaseType::DuckDb));
        assert!(supports_comments(DatabaseType::ClickHouse));
        assert!(supports_comments(DatabaseType::Oracle));
        assert!(!supports_comments(DatabaseType::Sqlite));
        assert!(!supports_comments(DatabaseType::LibSql));
        assert!(!supports_comments(DatabaseType::Redis));
        assert!(
            unsupported_reason(DatabaseType::Sqlite)
                .unwrap()
                .contains("no server-side comments")
        );
        assert!(
            unsupported_reason(DatabaseType::Redis)
                .unwrap()
                .contains("Redis")
        );
        assert!(unsupported_reason(DatabaseType::Postgres).is_none());
    }

    #[test]
    fn sql_server_adds_when_no_prior_description() {
        let sql = render_comment_change(
            DatabaseType::SqlServer,
            Some("sales"),
            "Orders",
            CommentObjectKind::Table,
            &change(None, None, Some("Customer orders")),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            sql,
            "EXEC sp_addextendedproperty @name = N'MS_Description', @value = N'Customer orders', @level0type = N'SCHEMA', @level0name = N'sales', @level1type = N'TABLE', @level1name = N'Orders';\n"
        );
    }

    #[test]
    fn sql_server_updates_when_description_existed() {
        let sql = render_comment_change(
            DatabaseType::SqlServer,
            Some("dbo"),
            "Orders",
            CommentObjectKind::Table,
            &change(None, Some("old"), Some("new")),
            None,
        )
        .unwrap()
        .unwrap();
        assert!(sql.starts_with("EXEC sp_updateextendedproperty"));
        assert!(sql.contains("@value = N'new'"));
    }

    #[test]
    fn sql_server_drops_when_cleared() {
        let sql = render_comment_change(
            DatabaseType::SqlServer,
            Some("dbo"),
            "Orders",
            CommentObjectKind::Table,
            &change(None, Some("old"), None),
            None,
        )
        .unwrap()
        .unwrap();
        assert!(sql.starts_with("EXEC sp_dropextendedproperty"));
        assert!(!sql.contains("@value"));
    }

    #[test]
    fn sql_server_column_level_and_view_level_types() {
        let column = render_comment_change(
            DatabaseType::SqlServer,
            Some("dbo"),
            "Orders",
            CommentObjectKind::Table,
            &change(Some("Status"), None, Some("Order status")),
            None,
        )
        .unwrap()
        .unwrap();
        assert!(column.contains("@level2type = N'COLUMN', @level2name = N'Status'"));
        let view = render_comment_change(
            DatabaseType::SqlServer,
            Some("dbo"),
            "vOrders",
            CommentObjectKind::View,
            &change(None, None, Some("Summary")),
            None,
        )
        .unwrap()
        .unwrap();
        assert!(view.contains("@level1type = N'VIEW'"));
    }

    #[test]
    fn sql_server_escapes_quotes_in_text_and_names() {
        let sql = render_comment_change(
            DatabaseType::SqlServer,
            Some("we'ird"),
            "Ord'ers",
            CommentObjectKind::Table,
            &change(None, None, Some("it's a ''quoted'' table")),
            None,
        )
        .unwrap()
        .unwrap();
        assert!(sql.contains("@value = N'it''s a ''''quoted'''' table'"));
        assert!(sql.contains("@level0name = N'we''ird'"));
        assert!(sql.contains("@level1name = N'Ord''ers'"));
    }

    #[test]
    fn postgres_comments_and_clears_with_null() {
        let set = render_comment_change(
            DatabaseType::Postgres,
            Some("public"),
            "orders",
            CommentObjectKind::Table,
            &change(None, None, Some("it's here")),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            set,
            "COMMENT ON TABLE \"public\".\"orders\" IS 'it''s here';\n"
        );
        let view = render_comment_change(
            DatabaseType::Postgres,
            Some("public"),
            "v_orders",
            CommentObjectKind::View,
            &change(None, None, Some("summary")),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            view,
            "COMMENT ON VIEW \"public\".\"v_orders\" IS 'summary';\n"
        );
        let clear = render_comment_change(
            DatabaseType::Postgres,
            Some("public"),
            "orders",
            CommentObjectKind::Table,
            &change(Some("status"), Some("x"), None),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            clear,
            "COMMENT ON COLUMN \"public\".\"orders\".\"status\" IS NULL;\n"
        );
    }

    #[test]
    fn oracle_uses_table_keyword_for_views_and_empty_string_clears() {
        let view = render_comment_change(
            DatabaseType::Oracle,
            Some("SALES"),
            "V_ORDERS",
            CommentObjectKind::View,
            &change(None, None, Some("summary")),
            None,
        )
        .unwrap()
        .unwrap();
        assert!(view.starts_with("COMMENT ON TABLE "));
        let clear = render_comment_change(
            DatabaseType::Oracle,
            Some("SALES"),
            "ORDERS",
            CommentObjectKind::Table,
            &change(Some("STATUS"), Some("x"), None),
            None,
        )
        .unwrap()
        .unwrap();
        assert!(clear.ends_with("IS '';\n"));
    }

    #[test]
    fn duckdb_comments_on_columns_and_views() {
        let col = render_comment_change(
            DatabaseType::DuckDb,
            Some("main"),
            "orders",
            CommentObjectKind::Table,
            &change(Some("status"), None, Some("lifecycle")),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            col,
            "COMMENT ON COLUMN \"main\".\"orders\".\"status\" IS 'lifecycle';\n"
        );
        let clear = render_comment_change(
            DatabaseType::DuckDb,
            Some("main"),
            "orders",
            CommentObjectKind::Table,
            &change(None, Some("x"), None),
            None,
        )
        .unwrap()
        .unwrap();
        assert!(clear.ends_with("IS NULL;\n"));
        let view = render_comment_change(
            DatabaseType::DuckDb,
            Some("main"),
            "v_orders",
            CommentObjectKind::View,
            &change(None, None, Some("summary")),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            view,
            "COMMENT ON VIEW \"main\".\"v_orders\" IS 'summary';\n"
        );
    }

    #[test]
    fn mysql_table_comment_and_clear() {
        let set = render_comment_change(
            DatabaseType::MySql,
            Some("sales"),
            "orders",
            CommentObjectKind::Table,
            &change(None, None, Some("it's a table \\ root")),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            set,
            "ALTER TABLE `sales`.`orders` COMMENT = 'it''s a table \\\\ root';\n"
        );
        let clear = render_comment_change(
            DatabaseType::MySql,
            Some("sales"),
            "orders",
            CommentObjectKind::Table,
            &change(None, Some("x"), None),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(clear, "ALTER TABLE `sales`.`orders` COMMENT = '';\n");
    }

    /// A `SHOW CREATE TABLE` body with every attribute the old draft-based
    /// restatement used to drop: charset/collation, ON UPDATE, a generated
    /// column, and an INVISIBLE flag.
    fn orders_ddl() -> &'static str {
        "CREATE TABLE `orders` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  `status` varchar(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL DEFAULT 'new' COMMENT 'x',\n  `created` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,\n  `total` decimal(10,2) GENERATED ALWAYS AS ((`price` * `qty`)) STORED,\n  `secret` int NOT NULL INVISIBLE,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4\n"
    }

    #[test]
    fn mysql_column_comment_restates_the_engine_definition() {
        let sql = render_comment_change(
            DatabaseType::MySql,
            Some("sales"),
            "orders",
            CommentObjectKind::Table,
            &change(Some("status"), None, Some("lifecycle")),
            Some(orders_ddl()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            sql,
            "ALTER TABLE `sales`.`orders` MODIFY COLUMN `status` varchar(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL DEFAULT 'new' COMMENT 'lifecycle';\n"
        );
        // Clearing removes the COMMENT clause but keeps the definition.
        let cleared = render_comment_change(
            DatabaseType::MySql,
            Some("sales"),
            "orders",
            CommentObjectKind::Table,
            &change(Some("status"), Some("x"), None),
            Some(orders_ddl()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            cleared,
            "ALTER TABLE `sales`.`orders` MODIFY COLUMN `status` varchar(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL DEFAULT 'new';\n"
        );
        // ON UPDATE survives: only the comment tail changes.
        let on_update = render_comment_change(
            DatabaseType::MariaDb,
            None,
            "orders",
            CommentObjectKind::Table,
            &change(Some("created"), None, Some("set")),
            Some(orders_ddl()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            on_update,
            "ALTER TABLE `orders` MODIFY COLUMN `created` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP COMMENT 'set';\n"
        );
        // A comment is appended to a definition that never had one.
        let appended = render_comment_change(
            DatabaseType::MySql,
            Some("sales"),
            "orders",
            CommentObjectKind::Table,
            &change(Some("id"), None, Some("key")),
            Some(orders_ddl()),
        )
        .unwrap()
        .unwrap();
        assert!(appended.contains("MODIFY COLUMN `id` int NOT NULL AUTO_INCREMENT COMMENT 'key';"));
    }

    #[test]
    fn mysql_stored_generated_comment_uses_inplace_and_preserves_definition() {
        let sql = render_comment_change(
            DatabaseType::MySql,
            Some("sales"),
            "orders",
            CommentObjectKind::Table,
            &change(Some("total"), None, Some("total amount")),
            Some(orders_ddl()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            sql,
            "ALTER TABLE `sales`.`orders` MODIFY COLUMN `total` decimal(10,2) GENERATED ALWAYS AS ((`price` * `qty`)) STORED COMMENT 'total amount', ALGORITHM=INPLACE;\n"
        );

        let persistent = render_comment_change(
            DatabaseType::MariaDb,
            Some("sales"),
            "orders",
            CommentObjectKind::Table,
            &change(Some("total"), None, Some("total amount")),
            Some(&orders_ddl().replace("STORED", "PERSISTENT")),
        )
        .unwrap()
        .unwrap();
        assert!(persistent.contains("PERSISTENT COMMENT 'total amount', ALGORITHM=INSTANT;"));
    }

    #[test]
    fn mariadb_native_persistent_shape_uses_instant() {
        let ddl = orders_ddl().replace(
            "`total` decimal(10,2) GENERATED ALWAYS AS ((`price` * `qty`)) STORED",
            "`total` decimal(10,2) AS ((`price` * `qty`)) PERSISTENT",
        );
        let sql = render_comment_change(
            DatabaseType::MariaDb,
            Some("sales"),
            "orders",
            CommentObjectKind::Table,
            &change(Some("total"), None, Some("total amount")),
            Some(&ddl),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            sql,
            "ALTER TABLE `sales`.`orders` MODIFY COLUMN `total` decimal(10,2) AS ((`price` * `qty`)) PERSISTENT COMMENT 'total amount', ALGORITHM=INSTANT;\n"
        );
    }

    #[test]
    fn mysql_ordinary_column_comment_does_not_force_instant() {
        let sql = render_comment_change(
            DatabaseType::MySql,
            Some("sales"),
            "orders",
            CommentObjectKind::Table,
            &change(Some("status"), None, Some("lifecycle")),
            Some(orders_ddl()),
        )
        .unwrap()
        .unwrap();
        assert!(!sql.contains("ALGORITHM="));
    }

    #[test]
    fn mysql_column_comment_refuses_without_the_table_ddl() {
        // No DDL fetched: an honest refusal beats a lossy restatement.
        let err = render_comment_change(
            DatabaseType::MySql,
            Some("sales"),
            "orders",
            CommentObjectKind::Table,
            &change(Some("status"), None, Some("lifecycle")),
            None,
        )
        .unwrap_err();
        assert!(err.contains("definition could not be read"), "{err}");
        assert!(err.contains("MODIFY COLUMN"), "{err}");
    }

    #[test]
    fn mysql_column_comment_refuses_when_column_missing_from_ddl() {
        let err = render_comment_change(
            DatabaseType::MySql,
            Some("sales"),
            "orders",
            CommentObjectKind::Table,
            &change(Some("gone"), None, Some("x")),
            Some(orders_ddl()),
        )
        .unwrap_err();
        assert!(err.contains("not found in the table's definition"), "{err}");
    }

    #[test]
    fn mysql_column_comment_never_matches_a_prefix_name() {
        // `id` must not match the `id` inside `id_ext`.
        let ddl = "CREATE TABLE `t` (\n  `id_ext` int NOT NULL,\n  PRIMARY KEY (`id_ext`)\n)";
        let err = render_comment_change(
            DatabaseType::MySql,
            None,
            "t",
            CommentObjectKind::Table,
            &change(Some("id"), None, Some("x")),
            Some(ddl),
        )
        .unwrap_err();
        assert!(err.contains("not found in the table's definition"), "{err}");
    }

    #[test]
    fn mysql_column_comment_survives_a_default_containing_the_word_comment() {
        let ddl = "CREATE TABLE `t` (\n  `note` varchar(9) NOT NULL DEFAULT 'COMMENT x' COMMENT 'y',\n  PRIMARY KEY (`note`)\n)";
        let sql = render_comment_change(
            DatabaseType::MySql,
            None,
            "t",
            CommentObjectKind::Table,
            &change(Some("note"), None, Some("new")),
            Some(ddl),
        )
        .unwrap()
        .unwrap();
        assert!(sql.contains("DEFAULT 'COMMENT x' COMMENT 'new';"), "{sql}");
    }

    #[test]
    fn mysql_column_comment_clear_is_a_noop_without_an_existing_comment() {
        let ddl = "CREATE TABLE `t` (\n  `note` varchar(9) NOT NULL,\n  PRIMARY KEY (`note`)\n)";
        let none = render_comment_change(
            DatabaseType::MySql,
            None,
            "t",
            CommentObjectKind::Table,
            &change(Some("note"), Some("x"), None),
            Some(ddl),
        )
        .unwrap();
        assert!(none.is_none(), "clearing an absent comment changes nothing");
    }

    #[test]
    fn mysql_column_comment_keeps_clauses_after_the_comment() {
        // MariaDB prints a JSON column's CHECK after its COMMENT; MySQL 8
        // prints ENGINE_ATTRIBUTE there. Both must survive the edit.
        let ddl = "CREATE TABLE `t` (\n  `doc` longtext CHARACTER SET utf8mb4 COLLATE utf8mb4_bin DEFAULT NULL COMMENT 'old' CHECK (json_valid(`doc`)),\n  `n` int DEFAULT NULL COMMENT 'n' ENGINE_ATTRIBUTE='{}'\n)";
        let doc = render_comment_change(
            DatabaseType::MariaDb,
            None,
            "t",
            CommentObjectKind::Table,
            &change(Some("doc"), Some("old"), Some("new")),
            Some(ddl),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            doc,
            "ALTER TABLE `t` MODIFY COLUMN `doc` longtext CHARACTER SET utf8mb4 COLLATE utf8mb4_bin DEFAULT NULL COMMENT 'new' CHECK (json_valid(`doc`));\n"
        );
        let cleared = render_comment_change(
            DatabaseType::MariaDb,
            None,
            "t",
            CommentObjectKind::Table,
            &change(Some("doc"), Some("old"), None),
            Some(ddl),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            cleared,
            "ALTER TABLE `t` MODIFY COLUMN `doc` longtext CHARACTER SET utf8mb4 COLLATE utf8mb4_bin DEFAULT NULL CHECK (json_valid(`doc`));\n"
        );
        let attr = render_comment_change(
            DatabaseType::MySql,
            None,
            "t",
            CommentObjectKind::Table,
            &change(Some("n"), Some("n"), Some("count")),
            Some(ddl),
        )
        .unwrap()
        .unwrap();
        assert!(
            attr.ends_with("`n` int DEFAULT NULL COMMENT 'count' ENGINE_ATTRIBUTE='{}';\n"),
            "{attr}"
        );
    }

    #[test]
    fn mysql_new_comment_goes_before_a_trailing_check() {
        let ddl = "CREATE TABLE `t` (\n  `doc` longtext DEFAULT NULL CHECK (json_valid(`doc`))\n)";
        let sql = render_comment_change(
            DatabaseType::MariaDb,
            None,
            "t",
            CommentObjectKind::Table,
            &change(Some("doc"), None, Some("json")),
            Some(ddl),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            sql,
            "ALTER TABLE `t` MODIFY COLUMN `doc` longtext DEFAULT NULL COMMENT 'json' CHECK (json_valid(`doc`));\n"
        );
    }

    #[test]
    fn mysql_quote_in_a_column_name_does_not_confuse_the_scanner() {
        // Before backticks were tracked, the `'` inside the name opened a
        // "string", hid the real COMMENT, and a second COMMENT was appended.
        let ddl = "CREATE TABLE `t` (\n  `it's` int NOT NULL COMMENT 'x'\n)";
        let sql = render_comment_change(
            DatabaseType::MySql,
            None,
            "t",
            CommentObjectKind::Table,
            &change(Some("it's"), Some("x"), Some("y")),
            Some(ddl),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            sql,
            "ALTER TABLE `t` MODIFY COLUMN `it's` int NOT NULL COMMENT 'y';\n"
        );
    }

    #[test]
    fn view_descriptions_are_gated_per_engine() {
        for db in [
            DatabaseType::MySql,
            DatabaseType::MariaDb,
            DatabaseType::ClickHouse,
            DatabaseType::CockroachDb,
        ] {
            assert!(
                unsupported_reason_for(db, CommentObjectKind::View).is_some(),
                "{db:?}"
            );
            assert!(
                unsupported_reason_for(db, CommentObjectKind::Table).is_none(),
                "{db:?}"
            );
            assert!(
                render_comment_change(
                    db,
                    Some("s"),
                    "v",
                    CommentObjectKind::View,
                    &change(None, None, Some("x")),
                    None,
                )
                .is_err(),
                "{db:?}"
            );
        }
        for db in [
            DatabaseType::Postgres,
            DatabaseType::SqlServer,
            DatabaseType::Oracle,
            DatabaseType::DuckDb,
        ] {
            assert!(unsupported_reason_for(db, CommentObjectKind::View).is_none());
        }
        assert!(unsupported_reason_for(DatabaseType::Sqlite, CommentObjectKind::Table).is_some());
    }

    #[test]
    fn mysql_view_is_an_honest_error() {
        let err = render_comment_change(
            DatabaseType::MySql,
            Some("sales"),
            "v_orders",
            CommentObjectKind::View,
            &change(None, None, Some("x")),
            None,
        )
        .unwrap_err();
        assert!(err.contains("cannot store a description on a view"));
    }

    #[test]
    fn clickhouse_modify_and_column_comment() {
        let table = render_comment_change(
            DatabaseType::ClickHouse,
            Some("analytics"),
            "events",
            CommentObjectKind::Table,
            &change(None, None, Some("all events")),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            table,
            "ALTER TABLE `analytics`.`events` MODIFY COMMENT 'all events';\n"
        );
        let col = render_comment_change(
            DatabaseType::ClickHouse,
            Some("analytics"),
            "events",
            CommentObjectKind::Table,
            &change(Some("kind"), Some("x"), None),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            col,
            "ALTER TABLE `analytics`.`events` COMMENT COLUMN `kind` '';\n"
        );
    }

    #[test]
    fn sqlite_and_redis_are_honest_errors() {
        assert!(
            render_comment_change(
                DatabaseType::Sqlite,
                None,
                "t",
                CommentObjectKind::Table,
                &change(None, None, Some("x")),
                None,
            )
            .is_err()
        );
        assert!(
            render_comment_change(
                DatabaseType::Redis,
                None,
                "t",
                CommentObjectKind::Table,
                &change(None, None, Some("x")),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn unchanged_is_a_no_op() {
        assert!(
            render_comment_change(
                DatabaseType::Postgres,
                Some("public"),
                "t",
                CommentObjectKind::Table,
                &change(None, Some("x"), Some("x")),
                None,
            )
            .unwrap()
            .is_none()
        );
    }
}
