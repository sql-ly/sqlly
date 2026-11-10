//! Pure cell-action helpers: what a field can do beyond typing, plus labels
//! and size caps. No SQL strings — dialect spelling lives in sqlly-services.

use sqlly_core::DatabaseType;
use sqlly_services::row_editing::{ColumnExpression, RowEditColumn};

use crate::capabilities;
use crate::cell_transform::CellTransform;
use crate::row_editor_model::{FieldKind, field_kind_for};

/// Grid / host cell action requested from the context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellAction {
    SetNull,
    SetDefault,
    ServerTime,
    SetBool(Option<bool>),
    /// Host-side enum write; the grid menu has no catalog so it is unused there.
    #[allow(dead_code)]
    SetEnum(String),
    LoadFromFile(std::path::PathBuf),
    /// Stage exact bytes (the value viewer's edited, re-compressed value).
    SetBytes(Vec<u8>),
    SaveToFile(std::path::PathBuf),
    /// Rewrite the cell's current text (staged value if any, else the
    /// original) through a transform macro.
    Transform(CellTransform),
    /// Fill a fresh client-generated random UUID.
    NewUuid,
    /// Fill the local clock's current date/time, shaped for the column kind.
    LocalTimestamp,
    /// Stage an engine-evaluated SQL function (server date, server UUID,
    /// current user). `Default`/`ServerTime` have their own variants.
    SqlFunction(ColumnExpression),
    /// Literal text typed in the Cell Details panel, parsed for the column's
    /// type like the row editor parses a field.
    SetText(String),
}

/// Maximum bytes accepted by "Load from file…" (matches the engine frame cap).
#[cfg(not(target_arch = "wasm32"))]
pub const MAX_BLOB_LOAD_BYTES: u64 = sqlly_remote::engine_protocol::MAX_FRAME_BYTES as u64;
/// Mirror of the native engine frame cap (`sqlly_remote::engine_protocol::MAX_FRAME_BYTES`,
/// 16 MiB) for the wasm build, where the engine-protocol crate is not linked.
#[cfg(target_arch = "wasm32")]
pub const MAX_BLOB_LOAD_BYTES: u64 = 16 * 1024 * 1024;

/// What a cell/field can do beyond typing text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellActionSet {
    pub set_null: bool,
    pub set_default: bool,
    pub server_time: bool,
    pub bool_choices: bool,
    pub enum_choices: Vec<String>,
    pub binary: bool,
    /// Transform macros (UPPER/lower/Title/trim/diacritics) apply: a free-text
    /// column without an enum constraint.
    pub transforms: bool,
    /// "New UUID" generator applies: a UUID column, or text wide enough for
    /// the 36-char canonical form.
    pub new_uuid: bool,
    /// Engine-evaluated function inserts the column accepts, in menu order.
    pub sql_functions: Vec<ColumnExpression>,
}

/// Whether `data_type` declares a binary column, from the one type table
/// the grid and the WHERE menu use ([`crate::result_cell_format::type_family`]).
/// `db` settles SQL Server's `timestamp` (its binary rowversion); with no
/// engine it is a date-time, as everywhere else.
pub fn is_binary_type_for(db: Option<DatabaseType>, data_type: &str) -> bool {
    crate::result_cell_format::type_family(db, data_type)
        == Some(crate::result_cell_format::TypeFamily::Binary)
}

/// The staged edit for text typed in the Cell Details panel, validated and
/// parsed exactly as the row editor does a field; `Err` carries the reason.
/// An empty string stays an empty string (Set NULL is separate). Binary
/// columns are written with Load from file, not as text.
pub fn text_cell_edit(
    db: DatabaseType,
    column: &RowEditColumn,
    text: &str,
) -> Result<crate::pending_changes::CellEdit, String> {
    let kind = field_kind_for(db, &column.data_type);
    if let Some(reason) = crate::row_editor_model::column_read_only_reason(column, kind, false) {
        return Err(reason);
    }
    if kind == FieldKind::Binary {
        return Err(crate::localization::text(
            "results.cell_details.read_only_binary",
        ));
    }
    if let Some(error) = crate::row_editor_model::validate_text(text, kind, column) {
        return Err(crate::localization::format(
            "shell.cells.text_rejected",
            &[("column", &column.name), ("error", &error)],
        ));
    }
    Ok(crate::pending_changes::CellEdit::Value(
        crate::row_editor_model::coerce_text(text, kind),
    ))
}

pub fn actions_for(db: DatabaseType, column: &RowEditColumn, editable: bool) -> CellActionSet {
    let kind = field_kind_for(db, &column.data_type);
    let binary = kind == FieldKind::Binary || is_binary_type_for(Some(db), &column.data_type);
    let enum_free = column.enum_values.is_empty();
    // `field_kind` edits JSON/XML/ENUM/SET as text (the row editor needs
    // that), but free-text rewrites and generated strings are only safe on
    // plain character columns: a UUID is not valid JSON, and an upper-cased
    // ENUM value is invalid (MySQL non-strict mode stores '').
    let plain_text = kind == FieldKind::Text
        && !binary
        && sqlly_services::row_editing::is_plain_text_column(column);
    let free_text = editable && plain_text;
    let uuid_capable = editable
        && enum_free
        && (kind == FieldKind::Guid
            || (plain_text
                && column
                    .char_max_length
                    .is_none_or(|len| !(0..36).contains(&len))));
    let mut sql_functions = Vec::new();
    if editable && matches!(kind, FieldKind::Date | FieldKind::DateTime) {
        sql_functions.push(ColumnExpression::ServerDate);
    }
    if uuid_capable {
        sql_functions.push(ColumnExpression::ServerUuid);
    }
    if free_text
        && sqlly_services::row_editing_sql::expression_supported(db, ColumnExpression::CurrentUser)
    {
        sql_functions.push(ColumnExpression::CurrentUser);
    }
    CellActionSet {
        transforms: free_text,
        new_uuid: uuid_capable,
        sql_functions,
        set_null: column.nullable && editable,
        set_default: column.default_definition.is_some()
            && editable
            && capabilities::cell_default_in_update(db).is_available(),
        server_time: editable
            && matches!(
                kind,
                FieldKind::Date | FieldKind::DateTime | FieldKind::Time
            ),
        bool_choices: kind == FieldKind::Bool,
        enum_choices: column.enum_values.clone(),
        binary,
    }
}

pub struct LocalizedReason(&'static str);

impl std::fmt::Display for LocalizedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::localization::text(self.0))
    }
}

/// Why the server-evaluated generators are unavailable on this Via-Relay
/// connection (menu tooltip and refusal text). Shown only when the bridge
/// did not advertise `row_edit.server_expressions`.
pub const RELAY_SQL_FUNCTIONS_REASON: LocalizedReason =
    LocalizedReason("results.cell_actions.relay_sql_functions_reason");

pub fn localized_relay_sql_functions_reason() -> String {
    crate::localization::text("results.cell_actions.relay_sql_functions_reason")
}

/// Whether a staged expression can travel to a bridge that did NOT advertise
/// `row_edit.server_expressions`. `ServerDate`/`ServerUuid`/`CurrentUser`
/// were added to the JSON wire enum after bridges shipped, and an older
/// bridge fails the whole request with "unknown variant".
pub fn expression_allowed_over_relay(expr: ColumnExpression) -> bool {
    !expr.needs_bridge_capability()
}

/// True when `profile` routes through a relay whose bridge engine has not
/// advertised the server-evaluated expressions (an older bridge, or one
/// whose `Ready` has not arrived). Direct routes never block. `services` must
/// be the ones resolved for `profile`: a relay session's row-editing service
/// answers from the capabilities its bridge advertised.
pub fn server_expressions_blocked(
    profile: &sqlly_core::ConnectionProfile,
    services: &sqlly_services::Services,
) -> bool {
    relay_blocks_server_expressions(
        matches!(
            profile.route,
            sqlly_core::connection::ConnectionRoute::ViaRelay { .. }
        ),
        services.row_editing.server_expressions_supported(),
    )
}

/// Why a staged grid save is unavailable on this Via-Relay connection.
/// Shown only when the bridge did not advertise `row_edit.apply_batch`.
pub const RELAY_STAGED_SAVE_REASON: LocalizedReason =
    LocalizedReason("results.cell_actions.relay_staged_save_reason");

pub fn localized_relay_staged_save_reason() -> String {
    crate::localization::text("results.cell_actions.relay_staged_save_reason")
}

/// True when `profile` routes through a relay whose bridge engine has not
/// advertised `row_edit.apply_batch`, so a staged grid save would fail.
/// Direct routes never block. `services` must be the ones resolved for
/// `profile`.
pub fn staged_save_blocked(
    profile: &sqlly_core::ConnectionProfile,
    services: &sqlly_services::Services,
) -> bool {
    relay_blocks_server_expressions(
        matches!(
            profile.route,
            sqlly_core::connection::ConnectionRoute::ViaRelay { .. }
        ),
        services.row_editing.batch_commit_supported(),
    )
}

/// The decision behind [`server_expressions_blocked`] and
/// [`staged_save_blocked`], separated for tests.
pub fn relay_blocks_server_expressions(via_relay: bool, bridge_supports: bool) -> bool {
    via_relay && !bridge_supports
}

impl CellActionSet {
    /// Drop the generators a bridge without `row_edit.server_expressions`
    /// cannot carry (see [`server_expressions_blocked`]). A no-op otherwise.
    #[must_use]
    pub fn for_route(mut self, server_expressions_blocked: bool) -> Self {
        if server_expressions_blocked {
            self.sql_functions
                .retain(|expr| expression_allowed_over_relay(*expr));
        }
        self
    }
}

/// Text shown in the grid/pending marker for a staged expression.
pub fn expression_label(expr: ColumnExpression) -> &'static str {
    match expr {
        ColumnExpression::Default => "DEFAULT",
        ColumnExpression::ServerTime => "<server time>",
        ColumnExpression::ServerDate => "<server date>",
        ColumnExpression::ServerUuid => "<server UUID>",
        ColumnExpression::CurrentUser => "<current user>",
    }
}

pub fn localized_expression_label(expr: ColumnExpression) -> String {
    if expr == ColumnExpression::Default {
        return "DEFAULT".to_string();
    }
    crate::localization::format(
        "results.cell_actions.expression_marker",
        &[("expression_name", &localized_function_name(expr))],
    )
}

fn localized_function_name(expr: ColumnExpression) -> String {
    crate::localization::text(match expr {
        ColumnExpression::Default => "results.cell_actions.default_function",
        ColumnExpression::ServerTime => "results.cell_actions.server_time_function",
        ColumnExpression::ServerDate => "results.cell_actions.server_date_function",
        ColumnExpression::ServerUuid => "results.cell_actions.server_uuid_function",
        ColumnExpression::CurrentUser => "results.cell_actions.current_user_function",
    })
}

/// Menu label for an engine-evaluated function insert, spelled with the
/// dialect's actual SQL so the user sees exactly what will be written.
pub fn sql_function_menu_label(db: DatabaseType, expr: ColumnExpression) -> String {
    crate::localization::format(
        "results.cell_actions.sql_function_menu_label",
        &[
            ("function_name", &localized_function_name(expr)),
            (
                "sql_expression",
                &sqlly_services::row_editing_sql::expression_sql(db, expr).to_string(),
            ),
        ],
    )
}

/// A fresh random (v4) UUID in canonical lowercase hyphenated form.
pub fn new_uuid_text() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The text a transform macro would stage for a cell, given its current
/// value (the staged edit when one exists, else the original). `None` when
/// the value is not text or the transform changes nothing, so the caller can
/// report "nothing to change" instead of staging a no-op edit.
pub fn transformed_cell_text(
    current: &sqlly_core::CellValue,
    transform: CellTransform,
) -> Option<String> {
    let sqlly_core::CellValue::String(text) = current else {
        return None;
    };
    let next = transform.apply(text);
    (next != *text).then_some(next)
}

/// Local "Today"/"Now" quick-pick text for a temporal field, in the ISO shapes
/// `draft_cell` already accepts.
pub fn quick_pick_now(kind: FieldKind) -> Option<String> {
    let now = chrono::Local::now();
    match kind {
        FieldKind::Date => Some(now.format("%Y-%m-%d").to_string()),
        FieldKind::Time => Some(now.format("%H:%M:%S").to_string()),
        FieldKind::DateTime => Some(now.format("%Y-%m-%d %H:%M:%S").to_string()),
        _ => None,
    }
}

pub fn blob_too_large(len: u64) -> Option<String> {
    if len <= MAX_BLOB_LOAD_BYTES {
        return None;
    }
    let mib = len as f64 / (1024.0 * 1024.0);
    Some(crate::localization::format(
        "results.cell_actions.binary_file_too_large",
        &[
            ("file_mib", &format!("{mib:.1}")),
            (
                "limit_mib",
                &(MAX_BLOB_LOAD_BYTES / (1024 * 1024)).to_string(),
            ),
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::CellValue;

    use crate::row_editor_model::{FieldDraft, FieldState};

    fn column(name: &str, data_type: &str) -> RowEditColumn {
        RowEditColumn {
            name: name.to_string(),
            data_type: data_type.to_string(),
            char_max_length: None,
            precision: None,
            scale: None,
            nullable: true,
            is_primary_key: false,
            is_identity: false,
            is_computed: false,
            is_rowversion: false,
            default_definition: None,
            result_ordinal: None,
            enum_values: Vec::new(),
        }
    }

    #[test]
    fn panel_text_edits_parse_like_the_row_editor() {
        use crate::pending_changes::CellEdit;
        let db = DatabaseType::SqlServer;
        assert_eq!(
            text_cell_edit(db, &column("name", "nvarchar"), ""),
            Ok(CellEdit::Value(CellValue::String(String::new()))),
            "empty text stays an empty string"
        );
        assert_eq!(
            text_cell_edit(db, &column("qty", "int"), "42"),
            Ok(CellEdit::Value(CellValue::I64(42)))
        );
        assert!(text_cell_edit(db, &column("qty", "int"), "lots").is_err());
        assert!(text_cell_edit(db, &column("blob", "varbinary"), "00").is_err());
        let mut key = column("id", "int");
        key.is_primary_key = true;
        assert!(text_cell_edit(db, &key, "7").is_err());
    }

    #[test]
    fn actions_bool_nullable() {
        let col = column("flag", "bit");
        let actions = actions_for(DatabaseType::SqlServer, &col, true);
        assert!(actions.bool_choices);
        assert!(actions.set_null);
        assert!(!actions.set_default);
        assert!(!actions.server_time);
        assert!(!actions.binary);
    }

    #[test]
    fn actions_default_needs_definition_and_capability() {
        let mut col = column("name", "nvarchar");
        col.default_definition = Some("('')".to_string());
        assert!(
            actions_for(DatabaseType::Postgres, &col, true).set_default,
            "postgres with a default is allowed"
        );
        assert!(
            !actions_for(DatabaseType::Sqlite, &col, true).set_default,
            "SQLite cannot SET col = DEFAULT on UPDATE"
        );
        col.default_definition = None;
        assert!(!actions_for(DatabaseType::Postgres, &col, true).set_default);
        col.default_definition = Some("('')".to_string());
        assert!(!actions_for(DatabaseType::Postgres, &col, false).set_default);
    }

    #[test]
    fn actions_enum_from_column() {
        let mut col = column("status", "enum");
        col.enum_values = vec!["open".into(), "closed".into()];
        let actions = actions_for(DatabaseType::MySql, &col, true);
        assert_eq!(actions.enum_choices, vec!["open", "closed"]);
    }

    #[test]
    fn binary_types() {
        assert!(is_binary_type_for(None, "bytea"));
        assert!(is_binary_type_for(None, "BLOB"));
        assert!(is_binary_type_for(None, "varbinary"));
        assert!(is_binary_type_for(None, "image"));
        assert!(!is_binary_type_for(None, "nvarchar"));
        let col = column("payload", "bytea");
        assert!(actions_for(DatabaseType::Postgres, &col, true).binary);
    }

    #[test]
    fn quick_pick_shapes_parse_back() {
        for kind in [FieldKind::Date, FieldKind::Time, FieldKind::DateTime] {
            let text = quick_pick_now(kind).expect("temporal kinds have a pick");
            let data_type = match kind {
                FieldKind::Date => "date",
                FieldKind::Time => "time",
                FieldKind::DateTime => "datetime2",
                _ => unreachable!(),
            };
            let field = FieldState {
                column: column("t", data_type),
                kind,
                original: CellValue::Null,
                draft: FieldDraft::Text(text.clone()),
                error: None,
                fk: None,
                fk_lead: false,
                fk_label: None,
                for_insert: false,
            };
            assert!(
                field.validation_error().is_none(),
                "{kind:?} pick {text:?} should parse"
            );
            let _ = field.draft_cell();
        }
    }

    #[test]
    fn blob_cap_message() {
        assert!(blob_too_large(MAX_BLOB_LOAD_BYTES).is_none());
        assert_eq!(
            blob_too_large(20 * 1024 * 1024).as_deref(),
            Some("File is 20.0 MiB; the limit is 16 MiB.")
        );
    }

    #[test]
    fn transforms_offered_only_on_editable_free_text() {
        let db = DatabaseType::SqlServer;
        assert!(actions_for(db, &column("name", "nvarchar"), true).transforms);
        assert!(!actions_for(db, &column("name", "nvarchar"), false).transforms);
        assert!(!actions_for(db, &column("n", "int"), true).transforms);
        assert!(!actions_for(db, &column("d", "datetime2"), true).transforms);
        assert!(!actions_for(db, &column("g", "uniqueidentifier"), true).transforms);
        let mut status = column("status", "enum");
        status.enum_values = vec!["open".into()];
        assert!(!actions_for(DatabaseType::MySql, &status, true).transforms);
    }

    #[test]
    fn rewrites_and_generators_skip_structured_and_set_columns() {
        // field_kind edits these as Text, but none of them is plain text.
        for (db, data_type) in [
            (DatabaseType::Postgres, "json"),
            (DatabaseType::Postgres, "jsonb"),
            (DatabaseType::SqlServer, "xml"),
            (DatabaseType::MySql, "set"),
            (DatabaseType::MySql, "enum"),
        ] {
            let actions = actions_for(db, &column("c", data_type), true);
            assert!(!actions.transforms, "{data_type}: transforms");
            assert!(!actions.new_uuid, "{data_type}: new UUID");
            assert!(
                actions.sql_functions.is_empty(),
                "{data_type}: {:?}",
                actions.sql_functions
            );
        }
    }

    #[test]
    fn relay_route_withholds_server_evaluated_generators() {
        use ColumnExpression::*;
        let db = DatabaseType::Postgres;
        let text = actions_for(db, &column("owner", "text"), true);
        assert_eq!(text.sql_functions, vec![ServerUuid, CurrentUser]);
        let relayed = text.clone().for_route(true);
        assert!(relayed.sql_functions.is_empty());
        assert!(relayed.new_uuid, "the client-side UUID still works");
        assert!(relayed.transforms);
        assert_eq!(text.clone().for_route(false), text);
        let date = actions_for(db, &column("d", "date"), true).for_route(true);
        assert!(date.sql_functions.is_empty());
        assert!(
            date.server_time,
            "ServerTime predates the relay wire format"
        );
        assert!(expression_allowed_over_relay(Default));
        assert!(expression_allowed_over_relay(ServerTime));
        assert!(!expression_allowed_over_relay(ServerDate));
    }

    /// The generators follow the bridge's advertised capability, not merely
    /// the route: a relay whose bridge supports them offers them, an old (or
    /// not-yet-announced) bridge does not, and Direct never blocks.
    #[test]
    fn server_expression_gate_follows_bridge_capability() {
        use sqlly_core::connection::ConnectionRoute;
        let mut profile = sqlly_core::ConnectionProfile::sql_login(
            "p",
            "localhost",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".to_string(),
            },
        );
        // A service that advertises nothing (the no-session bundle behaves
        // like an old bridge here).
        let old_bridge = sqlly_services::remote::unavailable_services("no session");
        // A service that supports them (what a current bridge reports).
        let capable = sqlly_services::Services::new();
        assert!(!server_expressions_blocked(&profile, &old_bridge));
        profile.route = ConnectionRoute::ViaRelay {
            relay_device_id: "dev_1".to_string(),
            pair_id: "pair_1".to_string(),
            relay_hint: None,
        };
        assert!(server_expressions_blocked(&profile, &old_bridge));
        assert!(!server_expressions_blocked(&profile, &capable));

        assert!(!staged_save_blocked(&profile, &capable));
        assert!(staged_save_blocked(&profile, &old_bridge));
        assert!(relay_blocks_server_expressions(true, false));
        assert!(!relay_blocks_server_expressions(true, true));
        assert!(!relay_blocks_server_expressions(false, false));
    }

    #[test]
    fn new_uuid_needs_uuid_or_wide_text() {
        let db = DatabaseType::Postgres;
        assert!(actions_for(db, &column("id", "uuid"), true).new_uuid);
        assert!(actions_for(db, &column("t", "text"), true).new_uuid);
        let mut narrow = column("code", "varchar");
        narrow.char_max_length = Some(10);
        assert!(
            !actions_for(db, &narrow, true).new_uuid,
            "too narrow for 36 chars"
        );
        narrow.char_max_length = Some(36);
        assert!(actions_for(db, &narrow, true).new_uuid);
        narrow.char_max_length = Some(-1);
        assert!(actions_for(db, &narrow, true).new_uuid, "-1 is (max)");
        assert!(!actions_for(db, &column("n", "integer"), true).new_uuid);
        assert!(!actions_for(db, &column("id", "uuid"), false).new_uuid);
    }

    #[test]
    fn sql_functions_follow_column_kind_and_engine() {
        use ColumnExpression::*;
        assert_eq!(
            actions_for(DatabaseType::SqlServer, &column("d", "date"), true).sql_functions,
            vec![ServerDate]
        );
        assert_eq!(
            actions_for(
                DatabaseType::SqlServer,
                &column("g", "uniqueidentifier"),
                true
            )
            .sql_functions,
            vec![ServerUuid]
        );
        assert_eq!(
            actions_for(DatabaseType::Postgres, &column("owner", "text"), true).sql_functions,
            vec![ServerUuid, CurrentUser]
        );
        assert_eq!(
            actions_for(DatabaseType::Sqlite, &column("owner", "text"), true).sql_functions,
            vec![ServerUuid],
            "SQLite has no session user"
        );
        assert!(
            actions_for(DatabaseType::Postgres, &column("owner", "text"), false)
                .sql_functions
                .is_empty()
        );
        assert!(
            actions_for(DatabaseType::Postgres, &column("n", "integer"), true)
                .sql_functions
                .is_empty()
        );
    }

    #[test]
    fn sql_function_labels_show_dialect_sql() {
        assert_eq!(
            sql_function_menu_label(DatabaseType::SqlServer, ColumnExpression::ServerUuid),
            "Server UUID (NEWID())"
        );
        assert_eq!(
            sql_function_menu_label(DatabaseType::Postgres, ColumnExpression::CurrentUser),
            "Current user (CURRENT_USER)"
        );
        assert_eq!(
            expression_label(ColumnExpression::ServerDate),
            "<server date>"
        );
    }

    #[test]
    fn new_uuid_text_is_canonical_v4() {
        let text = new_uuid_text();
        assert_eq!(text.len(), 36);
        let parsed = uuid::Uuid::parse_str(&text).expect("parses");
        assert_eq!(parsed.get_version_num(), 4);
        assert_ne!(new_uuid_text(), text);
    }

    #[test]
    fn transformed_cell_text_only_for_changed_strings() {
        assert_eq!(
            transformed_cell_text(&CellValue::String("ada".into()), CellTransform::Upper),
            Some("ADA".to_string())
        );
        assert_eq!(
            transformed_cell_text(&CellValue::String("ADA".into()), CellTransform::Upper),
            None
        );
        assert_eq!(
            transformed_cell_text(&CellValue::Null, CellTransform::Trim),
            None
        );
        assert_eq!(
            transformed_cell_text(&CellValue::I64(3), CellTransform::Lower),
            None
        );
    }

    #[test]
    fn cell_action_enum_variant_is_available() {
        let action = CellAction::SetEnum("open".into());
        assert_eq!(action, CellAction::SetEnum("open".into()));
        assert_ne!(action, CellAction::SetNull);
    }
}
