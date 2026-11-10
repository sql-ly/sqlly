use super::*;

pub(super) struct EditorContextMenu {
    pub(super) position: Point<Pixels>,
    pub(super) actions: Vec<EditorContextAction>,
    /// The popup menu entity rendered inside the anchored overlay.
    pub(super) menu: Entity<PopupMenu>,
    /// Keeps the menu's `DismissEvent` subscription alive while it is open;
    /// dropped (unsubscribing) when the menu closes.
    pub(super) _dismiss: Subscription,
}

/// A standard context-menu command: runs on the editor with the window.
pub(super) type ContextCommand = fn(&mut SqlEditor, &mut Window, &mut Context<SqlEditor>);

/// One entry of the context menu's standard section (see
/// `SqlEditor::standard_context_items`).
pub(super) enum StandardContextItem {
    Separator,
    Command {
        label: &'static str,
        enabled: bool,
        /// The editor action the command mirrors, when it has one: the menu
        /// shows that action's key binding next to the label.
        action: Option<Box<dyn gpui::Action>>,
        run: ContextCommand,
    },
    /// A named group (Edit, Find, Format, Folding) shown as a submenu.
    Submenu {
        label: &'static str,
        items: Vec<StandardContextItem>,
    },
}

impl Clone for StandardContextItem {
    fn clone(&self) -> Self {
        match self {
            Self::Separator => Self::Separator,
            Self::Command {
                label,
                enabled,
                action,
                run,
            } => Self::Command {
                label,
                enabled: *enabled,
                action: action.as_ref().map(|a| a.boxed_clone()),
                run: *run,
            },
            Self::Submenu { label, items } => Self::Submenu {
                label,
                items: items.clone(),
            },
        }
    }
}

impl StandardContextItem {
    /// Every command, through submenus — what the menu offers in all.
    #[cfg(test)]
    pub(super) fn flatten(items: Vec<StandardContextItem>) -> Vec<StandardContextItem> {
        let mut out = Vec::new();
        for item in items {
            match item {
                Self::Submenu { items, .. } => out.extend(Self::flatten(items)),
                other => out.push(other),
            }
        }
        out
    }
}

/// Whether an action belongs in the Automagic submenu (the per-query comma
/// and connector toggles) rather than among the contextual fixes.
pub(super) fn is_automagic_action(action: &EditorContextAction) -> bool {
    matches!(
        action,
        EditorContextAction::ToggleAutomagicComma { .. }
            | EditorContextAction::ToggleAutomagicConnectors { .. }
            | EditorContextAction::ResetAutomagicCommas
    )
}

#[derive(Clone, Debug)]
pub(super) enum EditorContextAction {
    ExpandWildcard {
        sql: String,
        cursor: usize,
    },
    AddJoin {
        sql: String,
    },
    ShowDisplayName {
        sql: String,
    },
    FormatColumn {
        column: String,
        kind: sqlly_lang::sqlmodel::TypeClass,
    },
    QualifyIdentifier {
        sql: String,
        cursor: usize,
        qualifier: String,
        column: String,
    },
    UnqualifyIdentifier {
        sql: String,
        cursor: usize,
        column: String,
    },
    QualifyAllInSelection {
        sql: String,
        count: usize,
    },
    /// Wrap the INSERT under the caret in `SET IDENTITY_INSERT <table>
    /// ON`/`OFF`, offered on the column the IDENTITY_INSERT lint flagged.
    EnableIdentityInsert {
        sql: String,
        cursor: usize,
        table: String,
    },
    ToggleAutomagicComma {
        clause: ListClause,
        enabled: bool,
    },
    ToggleAutomagicConnectors {
        enabled: bool,
    },
    ResetAutomagicCommas,
}

#[derive(Clone)]
pub(super) struct ColumnFormatDialog {
    pub(super) column: String,
    pub(super) choices: Vec<ColumnFormatChoice>,
    /// Keyboard-highlighted choice; Up/Down move it, Enter applies it.
    pub(super) selected: usize,
}

#[derive(Clone)]
pub(super) struct ColumnFormatChoice {
    pub(super) label: String,
    pub(super) directive: String,
}

pub(super) fn editor_context_action_label(action: &EditorContextAction) -> String {
    use crate::localization::{format, text};
    match action {
        EditorContextAction::ExpandWildcard { .. } => text("editor.context_menu.expand_wildcard"),
        EditorContextAction::AddJoin { .. } => text("editor.context_menu.add_foreign_key_join"),
        EditorContextAction::ShowDisplayName { .. } => {
            text("editor.context_menu.show_display_name")
        }
        EditorContextAction::FormatColumn { column, .. } => format(
            "editor.context_menu.format_column",
            &[("column", column.as_str())],
        ),
        EditorContextAction::QualifyIdentifier {
            qualifier, column, ..
        } => format(
            "editor.context_menu.qualify_identifier",
            &[
                ("qualifier", qualifier.as_str()),
                ("column", column.as_str()),
            ],
        ),
        EditorContextAction::UnqualifyIdentifier { column, .. } => format(
            "editor.context_menu.remove_qualifier",
            &[("column", column.as_str())],
        ),
        EditorContextAction::QualifyAllInSelection { count, .. } => {
            let key = if *count == 1 {
                "editor.context_menu.qualify_selection_one"
            } else {
                "editor.context_menu.qualify_selection_many"
            };
            format(key, &[("count", count)])
        }
        EditorContextAction::EnableIdentityInsert { table, .. } => format(
            "editor.context_menu.identity_insert_on",
            &[("table", table.as_str())],
        ),
        EditorContextAction::ToggleAutomagicComma { clause, enabled } => {
            let key = if *enabled {
                "editor.context_menu.automagic_commas_disable"
            } else {
                "editor.context_menu.automagic_commas_enable"
            };
            format(key, &[("clause", clause.label())])
        }
        EditorContextAction::ToggleAutomagicConnectors { enabled } => text(if *enabled {
            "editor.context_menu.automagic_connectors_disable"
        } else {
            "editor.context_menu.automagic_connectors_enable"
        }),
        EditorContextAction::ResetAutomagicCommas => {
            text("editor.context_menu.automagic_commas_reset")
        }
    }
}

pub(super) fn editor_context_actions(
    sql: &str,
    offset: usize,
    selection: Option<Range<usize>>,
    snapshot: &sqlly_lang::sqlmodel::ModelSnapshot,
) -> Vec<EditorContextAction> {
    let context = sqlly_services::CompletionContext::auto(sql, sql.len());
    if let Some((range, qualifier)) = wildcard_at(sql, offset)
        && let Some((expanded_sql, cursor)) = expand_wildcard(
            sql,
            range,
            qualifier.as_deref(),
            snapshot,
            &context.table_refs,
        )
    {
        return vec![EditorContextAction::ExpandWildcard {
            sql: expanded_sql,
            cursor,
        }];
    }

    let mut actions = Vec::new();
    if let Some(range) = selection.filter(|range| range.start < range.end)
        && let Some((new_sql, count)) =
            qualify_all_in_range(sql, range, snapshot, &context.table_refs)
    {
        actions.push(EditorContextAction::QualifyAllInSelection {
            sql: new_sql,
            count,
        });
    }

    let Some((range, column_name, qualifier)) = column_at(sql, offset) else {
        return actions;
    };
    if qualifier.is_none() {
        for owner in column_owners(snapshot, &context.table_refs, &column_name) {
            let owner_qual = qualifier_spelling(sql, owner);
            let (new_sql, cursor) = qualify_identifier(sql, range.clone(), &owner_qual);
            actions.push(EditorContextAction::QualifyIdentifier {
                sql: new_sql,
                cursor,
                qualifier: sql_identifier(&owner_qual),
                column: column_name.clone(),
            });
        }
    } else if let Some(qual) = qualifier.as_deref()
        && resolve_table_ref(snapshot, &context.table_refs, Some(qual)).is_some()
        && column_owners(snapshot, &context.table_refs, &column_name).len() == 1
    {
        let (new_sql, cursor) = unqualify_identifier(sql, range.clone(), qual);
        actions.push(EditorContextAction::UnqualifyIdentifier {
            sql: new_sql,
            cursor,
            column: column_name.clone(),
        });
    }

    let Some(object) = resolve_table_ref(snapshot, &context.table_refs, qualifier.as_deref())
    else {
        return actions;
    };
    let Some(column) = object_column(object, &column_name) else {
        return actions;
    };

    if let Some(kind) = column.ty.as_ref().map(|ty| ty.class) {
        actions.push(EditorContextAction::FormatColumn {
            column: column.name.spelling().to_string(),
            kind,
        });
    }

    let sqlly_lang::sqlmodel::ObjectBody::Table(table) = &object.body else {
        return actions;
    };
    let Some(foreign_key) = table.foreign_keys.iter().find(|foreign_key| {
        foreign_key
            .columns
            .iter()
            .any(|name| name.matches(&column.name))
    }) else {
        return actions;
    };
    let Some(target) = snapshot
        .resolve(&foreign_key.target, None, None)
        .into_iter()
        .next()
    else {
        return actions;
    };
    let existing_reference = context
        .table_refs
        .iter()
        .find(|table_ref| table_ref_matches_object(table_ref, object))
        .and_then(|table_ref| table_ref.alias.clone())
        .unwrap_or_else(|| object.reference.display());
    let alias = unique_join_alias(target.reference.name.spelling(), &context.table_refs);
    let target_column = foreign_key
        .columns
        .iter()
        .position(|name| name.matches(&column.name))
        .and_then(|index| foreign_key.target_columns.get(index))
        .or_else(|| foreign_key.target_columns.first())
        .map(|name| name.spelling())
        .unwrap_or("Id");
    let join = format!(
        "LEFT JOIN {} {alias} ON {alias}.{} = {existing_reference}.{}",
        target.reference.display(),
        sql_identifier(target_column),
        sql_identifier(column.name.spelling())
    );
    let joined_sql = insert_join_clause(sql, &join);
    actions.push(EditorContextAction::AddJoin {
        sql: joined_sql.clone(),
    });

    if let Some(display_column) = object_columns(target)
        .into_iter()
        .find(|name| name.eq_ignore_ascii_case("name"))
    {
        let replacement = format!(
            "{alias}.{} AS {}",
            sql_identifier(&display_column),
            sql_identifier(&format!("{}_Name", column.name.spelling()))
        );
        let expression_start = qualifier.as_ref().map_or(range.start, |qualifier| {
            range.start.saturating_sub(qualifier.len() + 1)
        });
        let mut display_sql = String::with_capacity(sql.len() + replacement.len());
        display_sql.push_str(&sql[..expression_start]);
        display_sql.push_str(&replacement);
        display_sql.push_str(&sql[range.end..]);
        actions.push(EditorContextAction::ShowDisplayName {
            sql: insert_join_clause(&display_sql, &join),
        });
    }
    actions
}

pub(super) fn column_format_choices(
    column: &str,
    kind: sqlly_lang::sqlmodel::TypeClass,
) -> Vec<ColumnFormatChoice> {
    use sqlly_lang::sqlmodel::TypeClass;
    // (catalog key of the choice label, format directive)
    let directives: &[(&str, &str)] = match kind {
        TypeClass::DateTime => &[
            (
                "editor.format_column_dialog.choices.date_only",
                "date=%Y-%m-%d",
            ),
            (
                "editor.format_column_dialog.choices.date_time",
                "date=%Y-%m-%d %H:%M:%S",
            ),
            (
                "editor.format_column_dialog.choices.date_time_offset",
                "date=%Y-%m-%d %H:%M:%S %:z",
            ),
        ],
        TypeClass::Decimal | TypeClass::Float => &[
            (
                "editor.format_column_dialog.choices.decimals_two",
                "number=2; thousands=true",
            ),
            (
                "editor.format_column_dialog.choices.decimals_four",
                "number=4; thousands=true",
            ),
            (
                "editor.format_column_dialog.choices.decimals_six",
                "number=6; thousands=true",
            ),
            (
                "editor.format_column_dialog.choices.decimals_no_grouping",
                "number=2; thousands=false",
            ),
        ],
        TypeClass::Integer => &[
            (
                "editor.format_column_dialog.choices.integer_thousands",
                "number=0; thousands=true",
            ),
            (
                "editor.format_column_dialog.choices.integer_plain",
                "number=0; thousands=false",
            ),
            (
                "editor.format_column_dialog.choices.epoch_seconds",
                "epoch=s",
            ),
            (
                "editor.format_column_dialog.choices.epoch_milliseconds",
                "epoch=ms",
            ),
        ],
        TypeClass::Boolean => &[
            (
                "editor.format_column_dialog.choices.boolean_true_false",
                "boolean=true/false",
            ),
            (
                "editor.format_column_dialog.choices.boolean_yes_no",
                "boolean=Yes/No",
            ),
            (
                "editor.format_column_dialog.choices.boolean_one_zero",
                "boolean=1/0",
            ),
        ],
        TypeClass::Binary => &[
            (
                "editor.format_column_dialog.choices.binary_hex",
                "binary=hex",
            ),
            (
                "editor.format_column_dialog.choices.binary_base64",
                "binary=base64",
            ),
            (
                "editor.format_column_dialog.choices.binary_uuid",
                "binary=uuid",
            ),
            (
                "editor.format_column_dialog.choices.binary_ulid",
                "binary=ulid",
            ),
        ],
        _ => &[
            (
                "editor.format_column_dialog.choices.text_original",
                "text=none",
            ),
            (
                "editor.format_column_dialog.choices.text_uppercase",
                "text=upper",
            ),
            (
                "editor.format_column_dialog.choices.text_lowercase",
                "text=lower",
            ),
            (
                "editor.format_column_dialog.choices.text_mask_last_four",
                "mask=0/4",
            ),
            (
                "editor.format_column_dialog.choices.text_mask_all",
                "mask=0/0",
            ),
        ],
    };
    let mut choices = Vec::with_capacity(directives.len() + 1);
    for (label_key, format) in directives {
        choices.push(ColumnFormatChoice {
            label: crate::localization::text(label_key),
            directive: format!("-- sqlly format: column={column}; {format}"),
        });
    }
    choices.push(ColumnFormatChoice {
        label: crate::localization::text("editor.format_column_dialog.choices.custom"),
        directive: "__custom__".into(),
    });
    choices
}

pub(super) fn result_column_type_class(data_type: &str) -> sqlly_lang::sqlmodel::TypeClass {
    use sqlly_lang::sqlmodel::TypeClass;
    match data_type
        .trim()
        .split(['(', ' '])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "date" | "datetime" | "datetime2" | "datetimeoffset" | "smalldatetime" | "time"
        | "timestamp" | "timestamptz" => TypeClass::DateTime,
        "decimal" | "numeric" | "money" | "smallmoney" | "number" => TypeClass::Decimal,
        "float" | "real" | "double" => TypeClass::Float,
        "tinyint" | "smallint" | "int" | "integer" | "bigint" | "serial" | "bigserial" => {
            TypeClass::Integer
        }
        "bit" | "bool" | "boolean" => TypeClass::Boolean,
        "binary" | "varbinary" | "image" | "bytea" | "blob" => TypeClass::Binary,
        _ => TypeClass::Character,
    }
}

pub(super) fn update_or_insert_format_comment(sql: &str, column: &str, directive: &str) -> String {
    // Recognize existing rules with the shared lexer plus the same first-rule
    // shape `query_format::parse_body` reads (`column=` key is case-sensitive
    // there, the value is not), so any line the parser honors for this column
    // is replaced in place rather than left behind to override the new rule.
    let is_rule_for_column = |line: &str| match crate::sqlly_directive::classify(line.trim()) {
        Some(crate::sqlly_directive::Directive::Format { body }) => body
            .split(';')
            .next()
            .and_then(|first| first.trim().strip_prefix("column="))
            .is_some_and(|value| value.trim().eq_ignore_ascii_case(column)),
        _ => false,
    };
    let mut lines: Vec<String> = sql.lines().map(str::to_string).collect();
    if let Some(index) = lines.iter().position(|line| is_rule_for_column(line)) {
        lines[index] = directive.to_string();
    } else {
        let insert_at = lines
            .iter()
            .take_while(|line| line.trim().is_empty() || line.trim_start().starts_with("--"))
            .count();
        lines.insert(insert_at, directive.to_string());
    }
    let mut updated = lines.join("\n");
    if sql.ends_with('\n') {
        updated.push('\n');
    }
    updated
}

pub(super) struct ConvertListDialog {
    pub(super) source: String,
    pub(super) output_delimiter: Entity<InputState>,
    pub(super) prefix: Entity<InputState>,
    pub(super) suffix: Entity<InputState>,
    pub(super) wrap_width: Entity<InputState>,
    pub(super) input_delimiter: crate::list_tools::InputDelimiter,
    pub(super) quote: crate::list_tools::Quote,
    pub(super) dedupe: bool,
    pub(super) _subscriptions: Vec<Subscription>,
}

impl ConvertListDialog {
    pub(super) fn new(source: String, window: &mut Window, cx: &mut Context<SqlEditor>) -> Self {
        let output_delimiter =
            cx.new(|cx| InputState::new(window, cx).default_value(", ".to_string()));
        let prefix = cx.new(|cx| InputState::new(window, cx));
        let suffix = cx.new(|cx| InputState::new(window, cx));
        let wrap_width = cx.new(|cx| InputState::new(window, cx).default_value("0".to_string()));
        let mut subscriptions = Vec::new();
        for input in [&output_delimiter, &prefix, &suffix, &wrap_width] {
            let input = input.clone();
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                |_: &mut SqlEditor, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ));
        }
        Self {
            source,
            output_delimiter,
            prefix,
            suffix,
            wrap_width,
            input_delimiter: crate::list_tools::InputDelimiter::Auto,
            quote: crate::list_tools::Quote::None,
            dedupe: false,
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn options(&self, cx: &App) -> crate::list_tools::ConvertOptions {
        crate::list_tools::ConvertOptions {
            input_delimiter: self.input_delimiter,
            output_delimiter: self.output_delimiter.read(cx).value().to_string(),
            quote: self.quote,
            prefix: self.prefix.read(cx).value().to_string(),
            suffix: self.suffix.read(cx).value().to_string(),
            wrap_width: self.wrap_width.read(cx).value().trim().parse().unwrap_or(0),
            trim: true,
            drop_empty: true,
            dedupe: self.dedupe,
        }
    }

    pub(super) fn preview(&self, cx: &App) -> Result<String, String> {
        crate::list_tools::convert_list(&self.source, &self.options(cx))
    }
}
