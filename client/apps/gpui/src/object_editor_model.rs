//! Pure model for the object editor dialogs. GPUI-free and fully unit-testable.
//!
//! One dialog shell serves tables, views, routines, indexes, statistics,
//! constraints, triggers, and user-defined types. This module owns:
//! - the per-kind page list and the [`MetadataQuery`] backing each page,
//! - parsers turning raw [`CellValue`] rows into typed, display-ready page
//!   data,
//! - the pending-change set staged by editing facets (renames, nullability
//!   toggles, statistic NORECOMPUTE), and
//! - the diff → DDL plan rendered in the Apply review sheet.
//!
//! Nothing here performs I/O; the view fetches rows through
//! `MetadataService::run_metadata_query` and hands them to
//! [`parse_page_data`].

use std::collections::BTreeMap;

use sqlly_core::CellValue;
use sqlly_intel::metadata::{MetadataQuery, escape_literal, quote_name};
use sqlly_services::alter::{CommentChange, CommentObjectKind, render_comment_change};

/// The object categories the editor supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorObjectKind {
    /// The connected instance itself. Its target carries no schema and no
    /// object name — the pages read server-scope catalogs.
    Server,
    Database,
    Schema,
    Table,
    View,
    Procedure,
    Function,
    Index,
    Statistic,
    Constraint,
    Trigger,
    Type,
    /// One column of a table or view. Its target carries the column name and
    /// `child_of` the parent table, the same shape indexes and triggers use.
    Column,
}

impl EditorObjectKind {
    /// Gutter marker matching the explorer tree's private dialect.
    pub fn marker(self) -> &'static str {
        match self {
            Self::Server => "SRV",
            Self::Database => "DB",
            Self::Schema => "SCH",
            Self::Table => "T",
            Self::View => "V",
            Self::Procedure => "SP",
            Self::Function => "FN",
            Self::Index => "IX",
            Self::Statistic => "ST",
            Self::Constraint => "CK",
            Self::Trigger => "TR",
            Self::Type => "TY",
            Self::Column => "C",
        }
    }

    pub fn noun(self) -> String {
        crate::localization::text(match self {
            Self::Server => "explorer.object_editor.kind_server",
            Self::Database => "explorer.object_editor.kind_database",
            Self::Schema => "explorer.object_editor.kind_schema",
            Self::Table => "explorer.object_editor.kind_table",
            Self::View => "explorer.object_editor.kind_view",
            Self::Procedure => "explorer.object_editor.kind_procedure",
            Self::Function => "explorer.object_editor.kind_function",
            Self::Index => "explorer.object_editor.kind_index",
            Self::Statistic => "explorer.object_editor.kind_statistic",
            Self::Constraint => "explorer.object_editor.kind_constraint",
            Self::Trigger => "explorer.object_editor.kind_trigger",
            Self::Type => "explorer.object_editor.kind_type",
            Self::Column => "explorer.object_editor.kind_column",
        })
    }

    /// Renaming is the v1 editable facet on the object itself. Types are
    /// read-only (a type change is drop/create; Script covers it).
    pub fn supports_rename(self) -> bool {
        matches!(
            self,
            Self::Table
                | Self::View
                | Self::Procedure
                | Self::Function
                | Self::Index
                | Self::Statistic
        )
    }

    /// `sp_rename` object-type category for this kind.
    fn rename_category(self) -> &'static str {
        match self {
            Self::Index => "INDEX",
            Self::Statistic => "STATISTICS",
            _ => "OBJECT",
        }
    }
}

/// The object the dialog is editing. Child objects retain their parent table
/// for context and the metadata queries that require it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorTarget {
    pub kind: EditorObjectKind,
    pub database: String,
    pub schema: String,
    pub name: String,
    pub parent: Option<String>,
}

impl EditorTarget {
    pub fn new(
        kind: EditorObjectKind,
        database: impl Into<String>,
        schema: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            database: database.into(),
            schema: schema.into(),
            name: name.into(),
            parent: None,
        }
    }

    pub fn child_of(mut self, parent: impl Into<String>) -> Self {
        self.parent = Some(parent.into());
        self
    }

    /// `schema.name` — the header's primary line.
    pub fn qualified_name(&self) -> String {
        if self.schema.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.schema, self.name)
        }
    }

    /// `schema.parent` for indexes/statistics — shown as secondary context.
    pub fn parent_context(&self) -> Option<String> {
        self.parent
            .as_ref()
            .map(|parent| format!("{}.{}", self.schema, parent))
    }

    /// The dotted name SQL Server uses to address this object. Indexes and
    /// statistics are addressed as `schema.table.object`; everything else as
    /// `schema.object`.
    fn engine_path(&self) -> String {
        match &self.parent {
            Some(parent) => format!("{}.{}.{}", self.schema, parent, self.name),
            None => format!("{}.{}", self.schema, self.name),
        }
    }

    /// The `schema.object` pair most metadata queries key on (parent table
    /// for indexes/statistics).
    fn query_object(&self) -> (String, String) {
        match (self.kind, &self.parent) {
            (EditorObjectKind::Index | EditorObjectKind::Statistic, Some(parent)) => {
                (self.schema.clone(), parent.clone())
            }
            _ => (self.schema.clone(), self.name.clone()),
        }
    }
}

/// Pages shared across kinds. Not every kind shows every page —
/// [`pages_for`] is the source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditorPage {
    General,
    /// Every database on the connected instance (server pages only).
    Databases,
    /// Server-wide settings (server pages only).
    Configuration,
    /// Server status counters (server pages only).
    Status,
    /// Storage a database occupies (database pages only).
    Files,
    Objects,
    Columns,
    Indexes,
    Statistics,
    Constraints,
    Triggers,
    Parameters,
    Definition,
    Permissions,
}

impl EditorPage {
    pub fn title(self) -> String {
        crate::localization::text(match self {
            Self::General => "explorer.object_editor.page_general",
            Self::Databases => "explorer.object_editor.page_databases",
            Self::Configuration => "explorer.object_editor.page_configuration",
            Self::Status => "explorer.object_editor.page_status",
            Self::Files => "explorer.object_editor.page_files",
            Self::Objects => "explorer.object_editor.page_objects",
            Self::Columns => "explorer.object_editor.page_columns",
            Self::Indexes => "explorer.object_editor.page_indexes",
            Self::Statistics => "explorer.object_editor.page_statistics",
            Self::Constraints => "explorer.object_editor.page_constraints",
            Self::Triggers => "explorer.object_editor.page_triggers",
            Self::Parameters => "explorer.object_editor.page_parameters",
            Self::Definition => "explorer.object_editor.page_definition",
            Self::Permissions => "explorer.object_editor.page_permissions",
        })
    }
}

/// How a page's content renders: scalar property rows, a data grid, or the
/// syntax-highlighted source pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageContent {
    Properties,
    Grid,
    Source,
}

/// One entry in the dialog's page list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageSpec {
    pub page: EditorPage,
    pub content: PageContent,
}

/// The page list for an object kind, in display order.
pub fn pages_for(kind: EditorObjectKind) -> Vec<PageSpec> {
    use EditorPage::*;
    use PageContent::{Grid, Properties, Source};
    let specs = match kind {
        EditorObjectKind::Server => vec![
            (General, Properties),
            (Databases, Grid),
            (Configuration, Grid),
            (Status, Grid),
            (Permissions, Grid),
        ],
        EditorObjectKind::Database => vec![
            (General, Properties),
            (Files, Grid),
            (Objects, Grid),
            (Permissions, Grid),
        ],
        EditorObjectKind::Column => vec![(General, Properties)],
        EditorObjectKind::Schema => vec![(General, Properties), (Objects, Grid)],
        EditorObjectKind::Table => vec![
            (General, Properties),
            (Columns, Grid),
            (Indexes, Grid),
            (Statistics, Grid),
            (Constraints, Grid),
            (Triggers, Grid),
            (Permissions, Grid),
        ],
        EditorObjectKind::View => vec![
            (General, Properties),
            (Columns, Grid),
            (Definition, Source),
            (Permissions, Grid),
        ],
        EditorObjectKind::Procedure | EditorObjectKind::Function => vec![
            (General, Properties),
            (Parameters, Grid),
            (Definition, Source),
            (Permissions, Grid),
        ],
        EditorObjectKind::Index => vec![(General, Properties), (Columns, Grid)],
        EditorObjectKind::Statistic => vec![(General, Properties), (Columns, Grid)],
        EditorObjectKind::Constraint => vec![(General, Properties), (Definition, Source)],
        EditorObjectKind::Trigger => vec![
            (General, Properties),
            (Definition, Source),
            (Permissions, Grid),
        ],
        EditorObjectKind::Type => vec![(General, Properties)],
    };
    specs
        .into_iter()
        .map(|(page, content)| PageSpec { page, content })
        .collect()
}

/// The metadata query backing a page for a concrete target.
pub fn query_for_page(target: &EditorTarget, page: EditorPage) -> MetadataQuery {
    let (schema, object) = target.query_object();
    match (target.kind, page) {
        (EditorObjectKind::Server, EditorPage::General) => MetadataQuery::ServerCapability,
        (EditorObjectKind::Server, EditorPage::Databases) => MetadataQuery::Databases,
        (EditorObjectKind::Server, EditorPage::Configuration) => MetadataQuery::ServerConfiguration,
        (EditorObjectKind::Server, EditorPage::Status) => MetadataQuery::ServerStatus,
        (EditorObjectKind::Server, EditorPage::Permissions) => MetadataQuery::ServerPermissions,
        (EditorObjectKind::Database, EditorPage::General) => MetadataQuery::DatabaseGeneral {
            database: target.database.clone(),
        },
        (EditorObjectKind::Database, EditorPage::Files) => MetadataQuery::DatabaseFiles {
            database: target.database.clone(),
        },
        // Every schema, not the (empty) schema field a database target carries.
        (EditorObjectKind::Database, EditorPage::Objects) => {
            MetadataQuery::ObjectInventory { schema: None }
        }
        (EditorObjectKind::Database, EditorPage::Permissions) => {
            MetadataQuery::DatabasePermissions {
                database: target.database.clone(),
            }
        }
        (EditorObjectKind::Column, EditorPage::General) => MetadataQuery::ColumnGeneral {
            schema,
            table: target.parent.clone().unwrap_or_default(),
            column: object,
        },
        (EditorObjectKind::Schema, EditorPage::General | EditorPage::Objects) => {
            MetadataQuery::ObjectInventory {
                schema: Some(target.name.clone()),
            }
        }
        (EditorObjectKind::Table, EditorPage::General) => MetadataQuery::TableGeneral {
            schema,
            table: object,
        },
        (EditorObjectKind::Type, EditorPage::General) => MetadataQuery::TypeDetail {
            schema,
            type_name: object,
        },
        (EditorObjectKind::Index, EditorPage::General) => MetadataQuery::IndexGeneral {
            schema,
            table: object,
            index: target.name.clone(),
        },
        (EditorObjectKind::Statistic, EditorPage::General) => MetadataQuery::StatisticGeneral {
            schema,
            table: object,
            statistic: target.name.clone(),
        },
        (EditorObjectKind::Constraint, EditorPage::Definition) => {
            MetadataQuery::ConstraintDefinition {
                schema,
                table: target.parent.clone().unwrap_or_default(),
                name: target.name.clone(),
            }
        }
        (EditorObjectKind::Trigger, EditorPage::Definition) => MetadataQuery::TriggerDefinition {
            schema,
            name: target.name.clone(),
        },
        (_, EditorPage::General) => MetadataQuery::ObjectGeneral { schema, object },
        (EditorObjectKind::Index, EditorPage::Columns) => MetadataQuery::IndexColumns {
            schema,
            table: object,
            index: target.name.clone(),
        },
        (EditorObjectKind::Statistic, EditorPage::Columns) => MetadataQuery::StatisticColumns {
            schema,
            table: object,
            statistic: target.name.clone(),
        },
        (_, EditorPage::Columns) => MetadataQuery::Columns { schema, object },
        (_, EditorPage::Indexes) => MetadataQuery::Indexes {
            schema,
            table: object,
        },
        (_, EditorPage::Statistics) => MetadataQuery::Statistics {
            schema,
            table: object,
        },
        (_, EditorPage::Constraints) => MetadataQuery::Constraints {
            schema,
            table: object,
        },
        (_, EditorPage::Triggers) => MetadataQuery::Triggers {
            schema,
            table: object,
        },
        (_, EditorPage::Parameters) => MetadataQuery::ProcedureParameters { schema, object },
        (_, EditorPage::Definition) => MetadataQuery::ObjectDefinition { schema, object },
        (_, EditorPage::Permissions) => MetadataQuery::ObjectPermissions { schema, object },
        (_, EditorPage::Objects) => MetadataQuery::ObjectInventory {
            schema: Some(schema),
        },
        (_, EditorPage::Databases) => MetadataQuery::Databases,
        (_, EditorPage::Configuration) => MetadataQuery::ServerConfiguration,
        (_, EditorPage::Status) => MetadataQuery::ServerStatus,
        (_, EditorPage::Files) => MetadataQuery::DatabaseFiles {
            database: target.database.clone(),
        },
    }
}

/// One label/value line on a General page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyRow {
    pub label: String,
    pub value: String,
    /// Untranslated source label (e.g. `"No recompute"`), so code can find a
    /// row without comparing against translated wording.
    pub source_label: String,
    /// The boolean behind a yes/no `value`, when the row is a flag.
    pub flag: Option<bool>,
}

fn property(label: &str, value: impl Into<String>) -> PropertyRow {
    PropertyRow {
        label: property_label(label),
        value: value.into(),
        source_label: label.to_string(),
        flag: None,
    }
}

fn flag_property(label: &str, value: bool) -> PropertyRow {
    PropertyRow {
        flag: Some(value),
        ..property(label, yes_no(value))
    }
}

/// Whether a statistic's General page reports NORECOMPUTE.
pub fn statistic_no_recompute(rows: &[PropertyRow]) -> bool {
    rows.iter()
        .find(|row| row.source_label == "No recompute")
        .and_then(|row| row.flag)
        .unwrap_or(false)
}

fn property_label(label: &str) -> String {
    let key = match label {
        "Schema" => "explorer.object_editor.property_schema",
        "Name" => "explorer.object_editor.property_name",
        "Created" => "explorer.object_editor.property_created",
        "Modified" => "explorer.object_editor.property_modified",
        "Rows (estimated)" => "explorer.object_editor.property_rows_estimated",
        "Machine name" => "explorer.object_editor.property_machine_name",
        "Product version" => "explorer.object_editor.property_product_version",
        "Edition" => "explorer.object_editor.property_edition",
        "Engine edition" => "explorer.object_editor.property_engine_edition",
        "Product level" => "explorer.object_editor.property_product_level",
        "Update level" => "explorer.object_editor.property_update_level",
        "Authentication" => "explorer.object_editor.property_authentication",
        "State" => "explorer.object_editor.property_state",
        "Collation" => "explorer.object_editor.property_collation",
        "Recovery model" => "explorer.object_editor.property_recovery_model",
        "Compatibility level" => "explorer.object_editor.property_compatibility_level",
        "Size" => "explorer.object_editor.property_size",
        "Owner" => "explorer.object_editor.property_owner",
        "Table" => "explorer.object_editor.property_table",
        "Position" => "explorer.object_editor.property_position",
        "Type" => "explorer.object_editor.property_type",
        "Nullable" => "explorer.object_editor.property_nullable",
        "Default" => "explorer.object_editor.property_default",
        "Identity" => "explorer.object_editor.property_identity",
        "Computed" => "explorer.object_editor.property_computed",
        "Objects" => "explorer.object_editor.property_objects",
        "Includes" => "explorer.object_editor.property_includes",
        "Base type" => "explorer.object_editor.property_base_type",
        "Unique" => "explorer.object_editor.property_unique",
        "Primary key" => "explorer.object_editor.property_primary_key",
        "Disabled" => "explorer.object_editor.property_disabled",
        "Filter" => "explorer.object_editor.property_filter",
        "Fill factor" => "explorer.object_editor.property_fill_factor",
        "Auto created" => "explorer.object_editor.property_auto_created",
        "User created" => "explorer.object_editor.property_user_created",
        "No recompute" => "explorer.object_editor.property_no_recompute",
        "Last updated" => "explorer.object_editor.property_last_updated",
        _ => return label.to_string(),
    };
    crate::localization::text(key)
}

fn yes_no(value: bool) -> String {
    crate::localization::text(if value {
        "explorer.object_editor.yes"
    } else {
        "explorer.object_editor.no"
    })
}

fn not_reported() -> String {
    crate::localization::text("explorer.object_editor.not_reported")
}

/// A table/view column, parsed from `MetadataQuery::Columns` rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub max_length: Option<i64>,
    pub precision: Option<i64>,
    pub scale: Option<i64>,
    pub is_nullable: bool,
    pub is_identity: bool,
    pub is_computed: bool,
}

impl ColumnInfo {
    /// Display form: `nvarchar(50)`, `decimal(18,2)`, `int`.
    pub fn display_type(&self) -> String {
        display_type(&self.type_name, self.max_length, self.precision, self.scale)
    }

    /// The type fragment an `ALTER TABLE … ALTER COLUMN` needs. Computed
    /// columns cannot be altered; the caller must refuse them.
    pub fn alter_type(&self) -> String {
        alter_type(&self.type_name, self.max_length, self.precision, self.scale)
    }

    /// Grid row: name, type, nullability, extras.
    pub fn grid_row(&self) -> Vec<String> {
        let mut extras = Vec::new();
        if self.is_identity {
            extras.push(crate::localization::text(
                "explorer.object_editor.flag_identity",
            ));
        }
        if self.is_computed {
            extras.push(crate::localization::text(
                "explorer.object_editor.flag_computed",
            ));
        }
        vec![
            self.name.clone(),
            self.display_type(),
            if self.is_nullable { "NULL" } else { "NOT NULL" }.to_string(),
            extras.join(", "),
        ]
    }
}

/// Display form for a SQL Server type with its length/precision metadata.
/// `sys.columns.max_length` is bytes: `n*` types count two bytes per char and
/// `-1` means `(max)`.
pub fn display_type(
    type_name: &str,
    max_length: Option<i64>,
    precision: Option<i64>,
    scale: Option<i64>,
) -> String {
    let lower = type_name.to_ascii_lowercase();
    match lower.as_str() {
        "nvarchar" | "nchar" => match max_length {
            Some(-1) => format!("{type_name}(max)"),
            Some(bytes) if bytes > 0 => format!("{type_name}({})", bytes / 2),
            _ => type_name.to_string(),
        },
        "varchar" | "char" | "varbinary" | "binary" => match max_length {
            Some(-1) => format!("{type_name}(max)"),
            Some(bytes) if bytes > 0 => format!("{type_name}({bytes})"),
            _ => type_name.to_string(),
        },
        "decimal" | "numeric" => match (precision, scale) {
            (Some(p), Some(s)) if p > 0 => format!("{type_name}({p},{s})"),
            _ => type_name.to_string(),
        },
        _ => type_name.to_string(),
    }
}

/// The type fragment used by `ALTER COLUMN`: bracket-quoted base type plus
/// the same length/precision decoration as [`display_type`].
pub fn alter_type(
    type_name: &str,
    max_length: Option<i64>,
    precision: Option<i64>,
    scale: Option<i64>,
) -> String {
    let lower = type_name.to_ascii_lowercase();
    let suffix = match lower.as_str() {
        "nvarchar" | "nchar" => match max_length {
            Some(-1) => "(max)".to_string(),
            Some(bytes) if bytes > 0 => format!("({})", bytes / 2),
            _ => String::new(),
        },
        "varchar" | "char" | "varbinary" | "binary" => match max_length {
            Some(-1) => "(max)".to_string(),
            Some(bytes) if bytes > 0 => format!("({bytes})"),
            _ => String::new(),
        },
        "decimal" | "numeric" => match (precision, scale) {
            (Some(p), Some(s)) if p > 0 => format!("({p},{s})"),
            _ => String::new(),
        },
        _ => String::new(),
    };
    format!("{}{}", quote_name(type_name), suffix)
}

/// An index row from `MetadataQuery::Indexes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    pub type_desc: String,
    pub is_unique: bool,
    pub is_primary_key: bool,
    pub filter_definition: Option<String>,
}

impl IndexInfo {
    pub fn grid_row(&self) -> Vec<String> {
        let flags = match (self.is_primary_key, self.is_unique) {
            (true, _) => "explorer.object_editor.flag_primary_key",
            (false, true) => "explorer.object_editor.flag_unique",
            _ => "",
        };
        vec![
            self.name.clone(),
            self.type_desc.clone(),
            if flags.is_empty() {
                String::new()
            } else {
                crate::localization::text(flags)
            },
            self.filter_definition.clone().unwrap_or_default(),
        ]
    }
}

/// A statistics row from `MetadataQuery::Statistics`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatisticInfo {
    pub name: String,
    pub auto_created: bool,
    pub user_created: bool,
    pub no_recompute: bool,
    pub updated_at: Option<String>,
}

impl StatisticInfo {
    pub fn grid_row(&self) -> Vec<String> {
        let origin = if self.auto_created {
            "explorer.object_editor.origin_auto"
        } else if self.user_created {
            "explorer.object_editor.origin_user"
        } else {
            "explorer.object_editor.origin_index"
        };
        vec![
            self.name.clone(),
            crate::localization::text(origin),
            yes_no(self.no_recompute),
            self.updated_at
                .clone()
                .unwrap_or_else(|| crate::localization::text("explorer.object_editor.never")),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintInfo {
    pub name: String,
    pub type_desc: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerInfo {
    pub name: String,
    pub is_disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterInfo {
    pub name: String,
    pub type_display: String,
    pub is_output: bool,
    pub has_default: bool,
}

/// One column of an index, key or included (`MetadataQuery::IndexColumns`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexColumnInfo {
    pub name: String,
    pub key_ordinal: Option<i64>,
    pub is_descending: bool,
    pub is_included: bool,
}

impl IndexColumnInfo {
    pub fn grid_row(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            crate::localization::text(if self.is_included {
                "explorer.object_editor.index_column_include"
            } else {
                "explorer.object_editor.index_column_key"
            }),
            self.key_ordinal
                .filter(|_| !self.is_included)
                .map(|n| n.to_string())
                .unwrap_or_default(),
            if !self.is_included && self.is_descending {
                crate::localization::text("explorer.object_editor.index_sort_descending")
            } else {
                String::new()
            },
        ]
    }
}

/// One database on the connected instance (`MetadataQuery::Databases`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseEntry {
    pub name: String,
    pub state: String,
    /// Engines without a compatibility level report none.
    pub compatibility_level: Option<String>,
}

impl DatabaseEntry {
    pub fn grid_row(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.state.clone(),
            self.compatibility_level.clone().unwrap_or_default(),
        ]
    }
}

/// One server-wide setting (`MetadataQuery::ServerConfiguration`) or status
/// counter (`MetadataQuery::ServerStatus`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingInfo {
    pub name: String,
    pub value: String,
    /// Where the value comes from, or what it means — whichever the engine
    /// records (SQL Server's description, PostgreSQL's source, …). On the
    /// Status page this is the counter's class/section.
    pub source: Option<String>,
    /// Whether the engine lets this value change at runtime from SQL — the
    /// Configuration page's Edit facet gates on it. Status counters are
    /// never dynamic.
    pub is_dynamic: bool,
    /// SQL Server "advanced" option: `sp_configure` refuses it (error 15123)
    /// unless `show advanced options` is on. Always false elsewhere.
    pub advanced: bool,
}

impl SettingInfo {
    pub fn grid_row(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.value.clone(),
            self.source.clone().unwrap_or_default(),
        ]
    }
}

/// The Configuration/Status search box: case-insensitive substring match on
/// name, value, or source. A blank query keeps every row.
pub fn filter_settings(settings: &[SettingInfo], query: &str) -> Vec<SettingInfo> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return settings.to_vec();
    }
    settings
        .iter()
        .filter(|setting| {
            setting.name.to_lowercase().contains(&needle)
                || setting.value.to_lowercase().contains(&needle)
                || setting
                    .source
                    .as_deref()
                    .is_some_and(|source| source.to_lowercase().contains(&needle))
        })
        .cloned()
        .collect()
}

/// Grid headers for a settings-shaped page: the Status page names its rows
/// counters and their grouping class.
pub fn settings_headers(page: EditorPage) -> Vec<String> {
    if page == EditorPage::Status {
        vec![
            crate::localization::text("explorer.object_editor.counter_column"),
            crate::localization::text("explorer.object_editor.value_column"),
            crate::localization::text("explorer.object_editor.class_column"),
        ]
    } else {
        vec![
            crate::localization::text("explorer.object_editor.setting_column"),
            crate::localization::text("explorer.object_editor.value_column"),
            crate::localization::text("explorer.object_editor.source_column"),
        ]
    }
}

/// Redis answers `INFO` through the query path as (section, key, value)
/// rows; the Status page's row shape is (name, value, source).
pub fn redis_status_rows(rows: &[Vec<CellValue>]) -> Vec<Vec<CellValue>> {
    rows.iter()
        .map(|row| {
            let pick = |ix: usize| row.get(ix).cloned().unwrap_or(CellValue::Null);
            vec![pick(1), pick(2), pick(0)]
        })
        .collect()
}

/// Empty-state copy for the Status page: engines without server-wide
/// counters say so plainly instead of showing a bare empty grid.
pub fn status_empty_copy(db: sqlly_lang::sqlmodel::DatabaseType) -> String {
    use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};
    crate::localization::text(match db.connection_kind() {
        ConnectionKind::SqliteFile => "explorer.object_editor.status_empty_sqlite",
        ConnectionKind::HranaHttp => "explorer.object_editor.status_empty_libsql",
        ConnectionKind::DuckDbFile => "explorer.object_editor.status_empty_duckdb",
        _ if db == DatabaseType::CockroachDb => "explorer.object_editor.status_empty_cockroach",
        _ => "explorer.object_editor.status_empty_default",
    })
}

/// One storage row of a database (`MetadataQuery::DatabaseFiles`). Engines
/// without files report a single summary row with empty path/max/growth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub logical_name: String,
    pub type_desc: String,
    pub physical_name: Option<String>,
    pub size_mb: Option<String>,
    pub max_size_mb: Option<String>,
    pub growth: Option<String>,
}

impl FileInfo {
    pub fn grid_row(&self) -> Vec<String> {
        let optional = |value: &Option<String>| value.clone().unwrap_or_default();
        vec![
            self.logical_name.clone(),
            self.type_desc.clone(),
            optional(&self.physical_name),
            optional(&self.size_mb),
            // "unlimited" is what a NULL max size means everywhere that
            // reports one; engines without the concept report no size either.
            match (&self.max_size_mb, &self.size_mb) {
                (Some(max), _) => max.clone(),
                (None, Some(_)) => crate::localization::text("explorer.object_editor.unlimited"),
                (None, None) => String::new(),
            },
            optional(&self.growth),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionInfo {
    pub permission: String,
    pub state: String,
    pub class: String,
}

/// Typed, display-ready data for one loaded page.
#[derive(Debug, Clone, PartialEq)]
pub enum PageData {
    Properties(Vec<PropertyRow>),
    Inventory(Vec<Vec<String>>),
    Databases(Vec<DatabaseEntry>),
    Settings(Vec<SettingInfo>),
    Files(Vec<FileInfo>),
    Columns(Vec<ColumnInfo>),
    Indexes(Vec<IndexInfo>),
    Statistics(Vec<StatisticInfo>),
    Constraints(Vec<ConstraintInfo>),
    Triggers(Vec<TriggerInfo>),
    Parameters(Vec<ParameterInfo>),
    IndexColumns(Vec<IndexColumnInfo>),
    StatisticColumns(Vec<String>),
    Definition(Option<String>),
    Permissions(Vec<PermissionInfo>),
}

impl PageData {
    /// Whether the loaded result set was empty (drives the page's empty
    /// state copy).
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Properties(rows) => rows.is_empty(),
            Self::Inventory(rows) => rows.is_empty(),
            Self::Databases(rows) => rows.is_empty(),
            Self::Settings(rows) => rows.is_empty(),
            Self::Files(rows) => rows.is_empty(),
            Self::Columns(rows) => rows.is_empty(),
            Self::Indexes(rows) => rows.is_empty(),
            Self::Statistics(rows) => rows.is_empty(),
            Self::Constraints(rows) => rows.is_empty(),
            Self::Triggers(rows) => rows.is_empty(),
            Self::Parameters(rows) => rows.is_empty(),
            Self::IndexColumns(rows) => rows.is_empty(),
            Self::StatisticColumns(rows) => rows.is_empty(),
            Self::Definition(def) => def.is_none(),
            Self::Permissions(rows) => rows.is_empty(),
        }
    }
}

/// Grid column headers for grid-shaped page data.
pub fn grid_columns(data: &PageData) -> Vec<String> {
    match data {
        PageData::Inventory(_) => vec![
            crate::localization::text("explorer.object_browser.column_kind"),
            crate::localization::text("explorer.object_browser.column_schema"),
            crate::localization::text("explorer.object_browser.column_name"),
            crate::localization::text("explorer.object_editor.rows_estimated_column"),
            crate::localization::text("explorer.object_browser.column_size"),
            crate::localization::text("explorer.object_browser.column_created"),
            crate::localization::text("explorer.object_browser.column_modified"),
            crate::localization::text("explorer.object_browser.column_comment"),
        ],
        PageData::Databases(_) => vec![
            crate::localization::text("explorer.object_editor.name_column"),
            crate::localization::text("explorer.object_editor.state_column"),
            crate::localization::text("explorer.object_editor.compatibility_column"),
        ],
        PageData::Settings(_) => vec![
            crate::localization::text("explorer.object_editor.setting_column"),
            crate::localization::text("explorer.object_editor.value_column"),
            crate::localization::text("explorer.object_editor.source_column"),
        ],
        PageData::Files(_) => vec![
            crate::localization::text("explorer.object_editor.logical_name_column"),
            crate::localization::text("explorer.object_editor.type_column"),
            crate::localization::text("explorer.object_editor.path_column"),
            crate::localization::text("explorer.object_editor.size_mb_column"),
            crate::localization::text("explorer.object_editor.max_size_mb_column"),
            crate::localization::text("explorer.object_editor.growth_column"),
        ],
        PageData::Columns(_) => vec![
            crate::localization::text("explorer.object_editor.name_column"),
            crate::localization::text("explorer.object_editor.type_column"),
            crate::localization::text("explorer.object_editor.nullability_column"),
            String::new(),
        ],
        PageData::Indexes(_) => vec![
            crate::localization::text("explorer.object_editor.name_column"),
            crate::localization::text("explorer.object_editor.type_column"),
            crate::localization::text("explorer.object_editor.flags_column"),
            crate::localization::text("explorer.object_editor.filter_column"),
        ],
        PageData::Statistics(_) => vec![
            crate::localization::text("explorer.object_editor.name_column"),
            crate::localization::text("explorer.object_editor.origin_column"),
            crate::localization::text("explorer.object_editor.no_recompute_column"),
            crate::localization::text("explorer.object_editor.last_updated_column"),
        ],
        PageData::Constraints(_) => vec![
            crate::localization::text("explorer.object_editor.name_column"),
            crate::localization::text("explorer.object_editor.type_column"),
        ],
        PageData::Triggers(_) => vec![
            crate::localization::text("explorer.object_editor.name_column"),
            crate::localization::text("explorer.object_editor.state_column"),
        ],
        PageData::Parameters(_) => vec![
            crate::localization::text("explorer.object_editor.name_column"),
            crate::localization::text("explorer.object_editor.type_column"),
            crate::localization::text("explorer.object_editor.direction_column"),
            crate::localization::text("explorer.object_editor.default_column"),
        ],
        PageData::IndexColumns(_) => vec![
            crate::localization::text("explorer.object_editor.name_column"),
            crate::localization::text("explorer.object_editor.index_kind_column"),
            crate::localization::text("explorer.object_editor.order_column"),
            crate::localization::text("explorer.object_editor.sort_column"),
        ],
        PageData::StatisticColumns(_) => {
            vec![crate::localization::text(
                "explorer.object_editor.column_column",
            )]
        }
        PageData::Permissions(_) => vec![
            crate::localization::text("explorer.object_editor.permission_column"),
            crate::localization::text("explorer.object_editor.state_column"),
            crate::localization::text("explorer.object_editor.class_column"),
        ],
        _ => Vec::new(),
    }
}

/// Display rows for grid-shaped page data, aligned with [`grid_columns`].
pub fn grid_rows(data: &PageData) -> Vec<Vec<String>> {
    match data {
        PageData::Inventory(rows) => rows.clone(),
        PageData::Databases(rows) => rows.iter().map(DatabaseEntry::grid_row).collect(),
        PageData::Settings(rows) => rows.iter().map(SettingInfo::grid_row).collect(),
        PageData::Files(rows) => rows.iter().map(FileInfo::grid_row).collect(),
        PageData::Columns(rows) => rows.iter().map(ColumnInfo::grid_row).collect(),
        PageData::Indexes(rows) => rows.iter().map(IndexInfo::grid_row).collect(),
        PageData::Statistics(rows) => rows.iter().map(StatisticInfo::grid_row).collect(),
        PageData::Constraints(rows) => rows
            .iter()
            .map(|c| vec![c.name.clone(), c.type_desc.clone()])
            .collect(),
        PageData::Triggers(rows) => rows
            .iter()
            .map(|t| {
                vec![
                    t.name.clone(),
                    if t.is_disabled {
                        crate::localization::text("explorer.object_editor.disabled")
                    } else {
                        crate::localization::text("explorer.object_editor.active")
                    },
                ]
            })
            .collect(),
        PageData::Parameters(rows) => rows
            .iter()
            .map(|p| {
                vec![
                    p.name.clone(),
                    p.type_display.clone(),
                    if p.is_output {
                        crate::localization::text("explorer.object_editor.output")
                    } else {
                        crate::localization::text("explorer.object_editor.input")
                    },
                    if p.has_default {
                        crate::localization::text("explorer.object_editor.yes")
                    } else {
                        String::new()
                    },
                ]
            })
            .collect(),
        PageData::IndexColumns(rows) => rows.iter().map(IndexColumnInfo::grid_row).collect(),
        PageData::StatisticColumns(rows) => rows.iter().map(|c| vec![c.clone()]).collect(),
        PageData::Permissions(rows) => rows
            .iter()
            .map(|p| vec![p.permission.clone(), p.state.clone(), p.class.clone()])
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse raw cell rows (from `MetadataService::run_metadata_query`) into the
/// typed page data for the target/page pair. Unknown `(kind, page)` pairings
/// yield an empty property list — the shell never asks for them.
pub fn parse_page_data(
    target: &EditorTarget,
    page: EditorPage,
    rows: &[Vec<CellValue>],
) -> PageData {
    match (target.kind, page) {
        (EditorObjectKind::Server, EditorPage::General) => {
            PageData::Properties(parse_server_general(&target.name, rows))
        }
        (EditorObjectKind::Database, EditorPage::General) => {
            PageData::Properties(parse_database_general(&target.name, rows))
        }
        (_, EditorPage::Databases) => PageData::Databases(
            rows.iter()
                .filter_map(|r| {
                    let name = cell_str(r.first())?;
                    Some(DatabaseEntry {
                        name,
                        state: cell_str(r.get(1)).unwrap_or_default(),
                        compatibility_level: cell_str(r.get(2)).filter(|v| !v.is_empty()),
                    })
                })
                .collect(),
        ),
        (_, EditorPage::Configuration) => PageData::Settings(
            rows.iter()
                .filter_map(|r| {
                    let name = cell_str(r.first())?;
                    Some(SettingInfo {
                        name,
                        value: cell_str(r.get(1)).unwrap_or_default(),
                        source: cell_str(r.get(2)).filter(|v| !v.is_empty()),
                        // The fourth column is the engine's dynamic flag; an
                        // engine that omits it reports read-only.
                        is_dynamic: cell_bool(r.get(3)).unwrap_or(false),
                        // SQL Server adds `is_advanced` as a fifth column.
                        advanced: cell_bool(r.get(4)).unwrap_or(false),
                    })
                })
                .collect(),
        ),
        // Status counters share the setting row shape (name, value, class);
        // they are never editable.
        (_, EditorPage::Status) => PageData::Settings(
            rows.iter()
                .filter_map(|r| {
                    let name = cell_str(r.first())?;
                    Some(SettingInfo {
                        name,
                        value: cell_str(r.get(1)).unwrap_or_default(),
                        source: cell_str(r.get(2)).filter(|v| !v.is_empty()),
                        is_dynamic: false,
                        advanced: false,
                    })
                })
                .collect(),
        ),
        (_, EditorPage::Files) => PageData::Files(
            rows.iter()
                .filter_map(|r| {
                    let logical_name = cell_str(r.first())?;
                    let optional = |ix: usize| cell_str(r.get(ix)).filter(|v| !v.is_empty());
                    Some(FileInfo {
                        logical_name,
                        type_desc: cell_str(r.get(1)).unwrap_or_default(),
                        physical_name: optional(2),
                        size_mb: optional(3),
                        max_size_mb: optional(4),
                        growth: optional(5),
                    })
                })
                .collect(),
        ),
        (EditorObjectKind::Schema, EditorPage::General) => {
            PageData::Properties(parse_schema_general(&target.name, rows))
        }
        (EditorObjectKind::Schema, EditorPage::Objects) => {
            PageData::Inventory(rows.iter().map(|row| inventory_row(row)).collect())
        }
        (EditorObjectKind::Table, EditorPage::General) => {
            PageData::Properties(parse_table_general(&target.schema, rows))
        }
        (EditorObjectKind::Column, EditorPage::General) => {
            PageData::Properties(parse_column_general(rows))
        }
        (EditorObjectKind::Type, EditorPage::General) => {
            PageData::Properties(parse_type_general(rows))
        }
        (EditorObjectKind::Index, EditorPage::General) => {
            PageData::Properties(parse_index_general(rows))
        }
        (EditorObjectKind::Statistic, EditorPage::General) => {
            PageData::Properties(parse_statistic_general(rows))
        }
        (_, EditorPage::General) => {
            PageData::Properties(parse_object_general(&target.schema, rows))
        }
        (EditorObjectKind::Index, EditorPage::Columns) => {
            PageData::IndexColumns(rows.iter().filter_map(|r| parse_index_column(r)).collect())
        }
        (EditorObjectKind::Statistic, EditorPage::Columns) => {
            PageData::StatisticColumns(rows.iter().filter_map(|r| cell_str(r.first())).collect())
        }
        (_, EditorPage::Columns) => {
            PageData::Columns(rows.iter().filter_map(|r| parse_column(r)).collect())
        }
        (_, EditorPage::Indexes) => {
            PageData::Indexes(rows.iter().filter_map(|r| parse_index(r)).collect())
        }
        (_, EditorPage::Statistics) => {
            PageData::Statistics(rows.iter().filter_map(|r| parse_statistic(r)).collect())
        }
        (_, EditorPage::Constraints) => PageData::Constraints(
            rows.iter()
                .filter_map(|r| {
                    let name = cell_str(r.first())?;
                    Some(ConstraintInfo {
                        name,
                        type_desc: cell_str(r.get(1)).unwrap_or_default(),
                    })
                })
                .collect(),
        ),
        (_, EditorPage::Triggers) => PageData::Triggers(
            rows.iter()
                .filter_map(|r| {
                    let name = cell_str(r.first())?;
                    Some(TriggerInfo {
                        name,
                        is_disabled: cell_bool(r.get(3)).unwrap_or(false),
                    })
                })
                .collect(),
        ),
        (_, EditorPage::Parameters) => {
            PageData::Parameters(rows.iter().filter_map(|r| parse_parameter(r)).collect())
        }
        (_, EditorPage::Definition) => PageData::Definition(
            rows.first()
                .and_then(|row| cell_str(row.first()))
                .filter(|def| !def.trim().is_empty()),
        ),
        (_, EditorPage::Permissions) => PageData::Permissions(
            rows.iter()
                .filter_map(|r| {
                    let permission = cell_str(r.first())?;
                    Some(PermissionInfo {
                        permission,
                        state: cell_str(r.get(1)).unwrap_or_default(),
                        class: cell_str(r.get(2)).unwrap_or_default(),
                    })
                })
                .collect(),
        ),
        (_, EditorPage::Objects) => {
            PageData::Inventory(rows.iter().map(|row| inventory_row(row)).collect())
        }
    }
}

fn parse_table_general(default_schema: &str, rows: &[Vec<CellValue>]) -> Vec<PropertyRow> {
    let Some(row) = rows.first() else {
        return Vec::new();
    };
    let schema = cell_str(row.first()).unwrap_or_else(|| default_schema.to_string());
    let name = cell_str(row.get(1)).unwrap_or_default();
    vec![
        property("Schema", schema),
        property("Name", name),
        property("Created", cell_str(row.get(2)).unwrap_or_default()),
        property("Modified", cell_str(row.get(3)).unwrap_or_default()),
        property(
            "Rows (estimated)",
            cell_i64(row.get(4))
                .map(|n| n.to_string())
                .unwrap_or_else(|| crate::localization::text("explorer.object_editor.unknown")),
        ),
    ]
}

/// The instance's own properties row (`MetadataQuery::ServerCapability`):
/// product version, edition, engine edition, product level, update level,
/// machine name, and whether the server only accepts integrated security.
///
/// Every engine returns all seven columns, NULL where it has no such concept,
/// so the page keeps one shape; `server` is the connection's own name, shown
/// when the engine reports no machine name of its own.
fn parse_server_general(server: &str, rows: &[Vec<CellValue>]) -> Vec<PropertyRow> {
    let Some(row) = rows.first() else {
        return Vec::new();
    };
    let field = |ix: usize| cell_str(row.get(ix)).filter(|value| !value.is_empty());
    let reported = |ix: usize| field(ix).unwrap_or_else(not_reported);
    vec![
        property(
            "Machine name",
            field(5).unwrap_or_else(|| server.to_string()),
        ),
        property("Product version", reported(0)),
        property("Edition", reported(1)),
        property("Engine edition", reported(2)),
        property("Product level", reported(3)),
        property("Update level", reported(4)),
        property(
            "Authentication",
            match cell_bool(row.get(6)) {
                Some(true) => crate::localization::text(
                    "explorer.object_editor.authentication_integrated_only",
                ),
                Some(false) => crate::localization::text(
                    "explorer.object_editor.authentication_sql_and_integrated",
                ),
                None => not_reported(),
            },
        ),
    ]
}

/// The database's own properties row: name, state, collation, recovery
/// model, compatibility level, creation date, size, owner.
///
/// Every engine returns all eight columns; the ones a given engine has no
/// concept of come back NULL and are reported as "not reported" rather than
/// being dropped, so the page has the same shape whatever you are connected
/// to.
fn parse_database_general(name: &str, rows: &[Vec<CellValue>]) -> Vec<PropertyRow> {
    let Some(row) = rows.first() else {
        return Vec::new();
    };
    let field = |ix: usize| cell_str(row.get(ix)).filter(|value| !value.is_empty());
    let reported = |ix: usize| field(ix).unwrap_or_else(not_reported);
    vec![
        property("Name", field(0).unwrap_or_else(|| name.to_string())),
        property("State", reported(1)),
        property("Collation", reported(2)),
        property("Recovery model", reported(3)),
        property("Compatibility level", reported(4)),
        property("Created", reported(5)),
        property(
            "Size",
            field(6)
                .map(|mb| format!("{mb} MB"))
                .unwrap_or_else(not_reported),
        ),
        property("Owner", reported(7)),
    ]
}

/// One column's properties row: position, type, size, nullability, default,
/// and the identity/computed/collation flags.
///
/// The type line is assembled rather than printed raw: an engine reports
/// `nvarchar` with a length in a separate column, and "nvarchar(200)" is what
/// the reader is actually looking for.
fn parse_column_general(rows: &[Vec<CellValue>]) -> Vec<PropertyRow> {
    let Some(row) = rows.first() else {
        return Vec::new();
    };
    let field = |ix: usize| cell_str(row.get(ix)).filter(|value| !value.is_empty());
    let number = |ix: usize| field(ix).and_then(|value| value.parse::<i64>().ok());
    let flag = |ix: usize| match number(ix) {
        Some(0) => yes_no(false),
        Some(_) => yes_no(true),
        None => not_reported(),
    };

    let base_type =
        field(4).unwrap_or_else(|| crate::localization::text("explorer.object_editor.unknown"));
    // MySQL's COLUMN_TYPE already reads `varchar(200)`; only decorate a bare
    // type name.
    let type_line = if base_type.contains('(') {
        base_type
    } else {
        match (number(5), number(6), number(7)) {
            // Precision/scale describe numerics; length describes strings.
            (_, Some(precision), Some(scale)) if precision > 0 && scale > 0 => {
                format!("{base_type}({precision},{scale})")
            }
            (_, Some(precision), _) if precision > 0 => format!("{base_type}({precision})"),
            (Some(-1), _, _) => format!("{base_type}(max)"),
            (Some(length), _, _) if length > 0 => format!("{base_type}({length})"),
            _ => base_type,
        }
    };

    let mut props = vec![
        property("Name", field(2).unwrap_or_default()),
        property("Table", field(1).unwrap_or_default()),
        property("Position", field(3).unwrap_or_else(not_reported)),
        property("Type", type_line),
        property(
            "Nullable",
            match number(8) {
                Some(0) => yes_no(false),
                Some(_) => yes_no(true),
                None => not_reported(),
            },
        ),
    ];
    // Only worth a line when the column actually has one.
    if let Some(default) = field(9) {
        props.push(property("Default", default));
    }
    props.push(property("Identity", flag(10)));
    props.push(property("Computed", flag(11)));
    if let Some(collation) = field(12) {
        props.push(property("Collation", collation));
    }
    props
}

fn parse_schema_general(name: &str, rows: &[Vec<CellValue>]) -> Vec<PropertyRow> {
    let mut counts = BTreeMap::<String, usize>::new();
    for row in rows {
        let kind = cell_str(row.first()).unwrap_or_else(|| "object".to_string());
        *counts.entry(kind).or_default() += 1;
    }
    let breakdown = counts
        .into_iter()
        .map(|(kind, count)| format!("{count} {kind}"))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        property("Name", name),
        property("Objects", rows.len().to_string()),
        property(
            "Includes",
            if breakdown.is_empty() {
                crate::localization::text("explorer.object_editor.no_loaded_objects")
            } else {
                breakdown
            },
        ),
    ]
}

fn inventory_row(row: &[CellValue]) -> Vec<String> {
    (0..8)
        .map(|index| cell_str(row.get(index)).unwrap_or_default())
        .collect()
}

/// The object's current server-side description, read from the rows of
/// `MetadataQuery::ObjectInventory` — the same catalog read whose Comment
/// column the explorer shows. Returns `None` when the object has no inventory
/// row or a NULL comment. An empty string is kept: on SQL Server it means an
/// `MS_Description` property exists with no text, so an edit must update it
/// rather than add a second one (every consumer reads `Some("")` as blank).
pub fn object_comment_from_inventory(
    rows: &[Vec<CellValue>],
    kind: EditorObjectKind,
    schema: &str,
    name: &str,
) -> Option<String> {
    let wanted_kind = match kind {
        EditorObjectKind::Table => "TABLE",
        EditorObjectKind::View => "VIEW",
        _ => return None,
    };
    for row in rows {
        let row_kind = cell_str(row.first()).unwrap_or_default();
        let row_schema = cell_str(row.get(1)).unwrap_or_default();
        let row_name = cell_str(row.get(2)).unwrap_or_default();
        if row_kind == wanted_kind
            && row_name == name
            && (schema.is_empty() || row_schema == schema)
        {
            return cell_str(row.get(7));
        }
    }
    None
}

fn parse_object_general(default_schema: &str, rows: &[Vec<CellValue>]) -> Vec<PropertyRow> {
    let Some(row) = rows.first() else {
        return Vec::new();
    };
    let schema = cell_str(row.first()).unwrap_or_else(|| default_schema.to_string());
    let name = cell_str(row.get(1)).unwrap_or_default();
    vec![
        property("Schema", schema),
        property("Name", name),
        property("Type", cell_str(row.get(2)).unwrap_or_default()),
        property("Created", cell_str(row.get(3)).unwrap_or_default()),
        property("Modified", cell_str(row.get(4)).unwrap_or_default()),
    ]
}

fn parse_type_general(rows: &[Vec<CellValue>]) -> Vec<PropertyRow> {
    let Some(row) = rows.first() else {
        return Vec::new();
    };
    let base = cell_str(row.get(2)).unwrap_or_default();
    let max_length = cell_i64(row.get(3));
    let precision = cell_i64(row.get(4));
    let scale = cell_i64(row.get(5));
    vec![
        property("Schema", cell_str(row.first()).unwrap_or_default()),
        property("Name", cell_str(row.get(1)).unwrap_or_default()),
        property(
            "Base type",
            display_type(&base, max_length, precision, scale),
        ),
        property("Nullable", yes_no(cell_bool(row.get(6)).unwrap_or(false))),
    ]
}

fn parse_index_general(rows: &[Vec<CellValue>]) -> Vec<PropertyRow> {
    let Some(row) = rows.first() else {
        return Vec::new();
    };
    vec![
        property("Name", cell_str(row.first()).unwrap_or_default()),
        property("Type", cell_str(row.get(1)).unwrap_or_default()),
        property("Unique", yes_no(cell_bool(row.get(2)).unwrap_or(false))),
        property(
            "Primary key",
            yes_no(cell_bool(row.get(3)).unwrap_or(false)),
        ),
        property("Disabled", yes_no(cell_bool(row.get(4)).unwrap_or(false))),
        property(
            "Filter",
            cell_str(row.get(5))
                .unwrap_or_else(|| crate::localization::text("explorer.object_editor.none")),
        ),
        property(
            "Fill factor",
            cell_i64(row.get(6))
                .map(|n| {
                    if n == 0 {
                        crate::localization::text("explorer.object_editor.fill_factor_default")
                    } else {
                        n.to_string()
                    }
                })
                .unwrap_or_default(),
        ),
    ]
}

fn parse_statistic_general(rows: &[Vec<CellValue>]) -> Vec<PropertyRow> {
    let Some(row) = rows.first() else {
        return Vec::new();
    };
    vec![
        property("Name", cell_str(row.first()).unwrap_or_default()),
        property(
            "Auto created",
            yes_no(cell_bool(row.get(1)).unwrap_or(false)),
        ),
        property(
            "User created",
            yes_no(cell_bool(row.get(2)).unwrap_or(false)),
        ),
        flag_property("No recompute", cell_bool(row.get(3)).unwrap_or(false)),
        property(
            "Last updated",
            cell_str(row.get(4))
                .unwrap_or_else(|| crate::localization::text("explorer.object_editor.never")),
        ),
    ]
}

fn parse_column(row: &[CellValue]) -> Option<ColumnInfo> {
    Some(ColumnInfo {
        name: cell_str(row.first())?,
        type_name: cell_str(row.get(1)).unwrap_or_default(),
        max_length: cell_i64(row.get(2)),
        precision: cell_i64(row.get(3)),
        scale: cell_i64(row.get(4)),
        is_nullable: cell_bool(row.get(5)).unwrap_or(false),
        is_identity: cell_bool(row.get(6)).unwrap_or(false),
        is_computed: cell_bool(row.get(7)).unwrap_or(false),
    })
}

fn parse_index(row: &[CellValue]) -> Option<IndexInfo> {
    Some(IndexInfo {
        name: cell_str(row.first())?,
        type_desc: cell_str(row.get(1)).unwrap_or_default(),
        is_unique: cell_bool(row.get(2)).unwrap_or(false),
        is_primary_key: cell_bool(row.get(3)).unwrap_or(false),
        filter_definition: cell_str(row.get(4)),
    })
}

fn parse_statistic(row: &[CellValue]) -> Option<StatisticInfo> {
    Some(StatisticInfo {
        name: cell_str(row.first())?,
        auto_created: cell_bool(row.get(1)).unwrap_or(false),
        user_created: cell_bool(row.get(2)).unwrap_or(false),
        no_recompute: cell_bool(row.get(3)).unwrap_or(false),
        updated_at: cell_str(row.get(4)),
    })
}

fn parse_parameter(row: &[CellValue]) -> Option<ParameterInfo> {
    let name = cell_str(row.first())?;
    // A procedure's return value surfaces as an unnamed parameter; skip it.
    if name.is_empty() {
        return None;
    }
    let type_name = cell_str(row.get(1)).unwrap_or_default();
    Some(ParameterInfo {
        name,
        type_display: display_type(
            &type_name,
            cell_i64(row.get(2)),
            cell_i64(row.get(3)),
            cell_i64(row.get(4)),
        ),
        is_output: cell_bool(row.get(5)).unwrap_or(false),
        has_default: cell_bool(row.get(6)).unwrap_or(false),
    })
}

fn parse_index_column(row: &[CellValue]) -> Option<IndexColumnInfo> {
    Some(IndexColumnInfo {
        name: cell_str(row.first())?,
        key_ordinal: cell_i64(row.get(1)),
        is_descending: cell_bool(row.get(2)).unwrap_or(false),
        is_included: cell_bool(row.get(3)).unwrap_or(false),
    })
}

/// The staged edit set. Each facet keeps "last write wins" semantics (staging
/// a column rename twice replaces the first draft), and staging a value equal
/// to the original clears the entry so the dialog can never report a
/// phantom-dirty state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PendingChanges {
    /// New object name, when the rename facet was staged.
    pub rename_object: Option<String>,
    /// Column renames: original name → new name.
    pub rename_columns: BTreeMap<String, String>,
    /// Nullability toggles: column name → (new nullability + alter fragment).
    pub column_nullability: BTreeMap<String, ColumnNullabilityChange>,
    /// Staged NORECOMPUTE value for a statistic dialog.
    pub statistic_no_recompute: Option<bool>,
    /// "Update statistics now" action armed.
    pub update_statistics_now: bool,
    /// Server setting edits staged from the Configuration page: setting name
    /// → (new value + the value to revert to + persist flag).
    pub server_settings: BTreeMap<String, ServerSettingChange>,
    /// The staged object description (server-side comment); an empty string
    /// clears the server's value. Tables and views only.
    pub description: Option<String>,
}

/// A staged server-setting edit. `revert` is the value the engine reported
/// when the page loaded — the review sheet shows it because none of the
/// engines' config statements are transactional: reverting means running the
/// same statement again with the old value. An empty `value` means "reset to
/// the server default" (`ALTER SYSTEM RESET`, `SET GLOBAL … = DEFAULT`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSettingChange {
    pub value: String,
    pub revert: String,
    /// MySQL 8 only: write with `SET PERSIST` (survives restart) instead of
    /// `SET GLOBAL` (until restart). Never offered on other engines.
    pub persist: bool,
    /// SQL Server only: the option is advanced and `show advanced options`
    /// was off when the page loaded, so the script must turn it on first
    /// (and back off afterwards).
    pub needs_advanced: bool,
}

impl ServerSettingChange {
    /// Whether this edit resets the setting to the server default.
    pub fn is_reset(&self) -> bool {
        self.value.trim().is_empty()
    }
}

/// The SQL Server option that unlocks the advanced ones.
pub const SHOW_ADVANCED_OPTIONS: &str = "show advanced options";

/// A staged nullability flip for one column. `alter_type` is captured when
/// the change is staged because `ALTER COLUMN` must restate the full type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnNullabilityChange {
    pub nullable: bool,
    pub alter_type: String,
}

impl PendingChanges {
    pub fn is_dirty(&self) -> bool {
        self.pending_count() > 0
    }

    pub fn pending_count(&self) -> usize {
        self.rename_object.iter().count()
            + self.rename_columns.len()
            + self.column_nullability.len()
            + self.statistic_no_recompute.iter().count()
            + usize::from(self.update_statistics_now)
            + self.server_settings.len()
            + self.description.iter().count()
    }

    /// Stage (or un-stage, when `new_name` equals the original) an object
    /// rename. Invalid names are rejected with a human-readable reason.
    pub fn stage_rename(&mut self, original: &str, new_name: &str) -> Result<(), String> {
        let new_name = new_name.trim();
        if new_name == original {
            self.rename_object = None;
            return Ok(());
        }
        validate_identifier(new_name)?;
        self.rename_object = Some(new_name.to_string());
        Ok(())
    }

    /// Stage (or un-stage) a column rename.
    pub fn stage_column_rename(
        &mut self,
        column: &str,
        original: &str,
        new_name: &str,
    ) -> Result<(), String> {
        let new_name = new_name.trim();
        if new_name == original {
            self.rename_columns.remove(column);
            return Ok(());
        }
        validate_identifier(new_name)?;
        self.rename_columns
            .insert(column.to_string(), new_name.to_string());
        Ok(())
    }

    /// Stage (or un-stage) a nullability flip.
    pub fn stage_nullability(&mut self, column: &ColumnInfo, nullable: bool) {
        if nullable == column.is_nullable {
            self.column_nullability.remove(&column.name);
            return;
        }
        self.column_nullability.insert(
            column.name.clone(),
            ColumnNullabilityChange {
                nullable,
                alter_type: column.alter_type(),
            },
        );
    }

    /// Stage (or un-stage) the NORECOMPUTE toggle.
    pub fn stage_no_recompute(&mut self, current: bool, staged: bool) {
        self.statistic_no_recompute = if staged == current {
            None
        } else {
            Some(staged)
        };
    }

    /// Stage (or un-stage, when `new_value` equals the current one) a server
    /// setting edit. Last write wins, like the other facets.
    pub fn stage_server_setting(
        &mut self,
        name: &str,
        current: &str,
        new_value: &str,
        persist: bool,
    ) {
        let new_value = new_value.trim();
        if new_value == current.trim() {
            self.server_settings.remove(name);
            return;
        }
        let needs_advanced = self
            .server_settings
            .get(name)
            .is_some_and(|change| change.needs_advanced);
        self.server_settings.insert(
            name.to_string(),
            ServerSettingChange {
                value: new_value.to_string(),
                revert: current.to_string(),
                persist,
                needs_advanced,
            },
        );
    }

    /// Flip only the persist flag of an already-staged setting edit.
    pub fn stage_server_setting_persist(&mut self, name: &str, persist: bool) {
        if let Some(change) = self.server_settings.get_mut(name) {
            change.persist = persist;
        }
    }

    /// Record that a staged SQL Server edit targets an advanced option while
    /// `show advanced options` is off (see [`ServerSettingChange::needs_advanced`]).
    pub fn mark_server_setting_advanced(&mut self, name: &str, needs_advanced: bool) {
        if let Some(change) = self.server_settings.get_mut(name) {
            change.needs_advanced = needs_advanced;
        }
    }

    /// Whether the settings script must switch `show advanced options` on
    /// around the staged edits (and restore it). Not when the user staged
    /// that option themselves — their explicit value wins.
    pub fn server_settings_toggle_advanced(&self) -> bool {
        !self.server_settings.contains_key(SHOW_ADVANCED_OPTIONS)
            && self
                .server_settings
                .values()
                .any(|change| change.needs_advanced)
    }

    /// Stage (or un-stage, when `staged` equals the server's current value)
    /// an object description edit. An empty staged string means "clear the
    /// description", which the per-engine DDL expresses in its own form.
    pub fn stage_description(&mut self, original: &str, staged: &str) {
        if staged == original {
            self.description = None;
        } else {
            self.description = Some(staged.to_string());
        }
    }

    /// One-line human summary per pending change, for the review sheet and
    /// the dirty-close confirm.
    pub fn summary_lines(&self, kind: EditorObjectKind) -> Vec<String> {
        let kind_label = kind.noun().to_lowercase();
        let mut lines = Vec::new();
        if let Some(new_name) = &self.rename_object {
            lines.push(crate::localization::format(
                "explorer.object_editor.summary_rename",
                &[("kind", &kind_label), ("name", new_name)],
            ));
        }
        for (column, new_name) in &self.rename_columns {
            lines.push(crate::localization::format(
                "explorer.object_editor.summary_rename_column",
                &[("column", column), ("name", new_name)],
            ));
        }
        for (column, change) in &self.column_nullability {
            let key = if change.nullable {
                "explorer.object_editor.summary_set_null"
            } else {
                "explorer.object_editor.summary_set_not_null"
            };
            lines.push(crate::localization::format(key, &[("column", column)]));
        }
        if let Some(no_recompute) = self.statistic_no_recompute {
            lines.push(crate::localization::text(if no_recompute {
                "explorer.object_editor.summary_disable_updates"
            } else {
                "explorer.object_editor.summary_enable_updates"
            }));
        }
        if self.update_statistics_now {
            lines.push(crate::localization::text(
                "explorer.object_editor.summary_update_statistics",
            ));
        }
        for (name, change) in &self.server_settings {
            lines.push(if change.is_reset() {
                crate::localization::format(
                    "explorer.object_editor.summary_reset_setting",
                    &[
                        ("name", name.as_str()),
                        ("previous", change.revert.as_str()),
                    ],
                )
            } else {
                crate::localization::format(
                    "explorer.object_editor.summary_set_setting",
                    &[
                        ("name", name.as_str()),
                        ("value", change.value.as_str()),
                        ("previous", change.revert.as_str()),
                    ],
                )
            });
        }
        if self.server_settings_toggle_advanced() {
            lines.push(crate::localization::format(
                "explorer.object_editor.summary_toggle_advanced",
                &[("option", SHOW_ADVANCED_OPTIONS)],
            ));
        }
        if !self.server_settings.is_empty() {
            // No engine wraps its config statements in a transaction: the
            // revert path is the old values above, re-applied by hand.
            lines.push(crate::localization::text(
                "explorer.object_editor.summary_settings_nontransactional",
            ));
        }
        if let Some(text) = &self.description {
            let key = if text.trim().is_empty() {
                "explorer.object_editor.summary_clear_description"
            } else {
                "explorer.object_editor.summary_set_description"
            };
            lines.push(crate::localization::format(key, &[("kind", &kind_label)]));
        }
        lines
    }
}

/// Validate a new identifier: non-empty after trimming and within SQL
/// Server's 128-character `sysname` limit. Bracket quoting in the generated
/// DDL handles every other character safely.
pub fn validate_identifier(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err(crate::localization::text(
            "explorer.object_editor.name_required",
        ));
    }
    if name.chars().count() > 128 {
        return Err(crate::localization::text(
            "explorer.object_editor.name_too_long",
        ));
    }
    Ok(())
}

/// Diff the pending set into the ordered T-SQL batch shown in the review
/// sheet (tests and legacy callers; prefer [`ddl_plan_for`]).
#[cfg(test)]
pub fn ddl_plan(
    target: &EditorTarget,
    changes: &PendingChanges,
    current_no_recompute: bool,
) -> Vec<String> {
    ddl_plan_for(
        sqlly_lang::sqlmodel::DatabaseType::SqlServer,
        target,
        changes,
        current_no_recompute,
        None,
    )
    .expect("T-SQL plan is always expressible")
}

/// Diff the pending set into the ordered statement batch shown in the review
/// sheet, in the ENGINE'S OWN forms. `current_no_recompute` is the
/// statistic's live flag so "Update statistics now" can preserve it;
/// `current_description` is the object's live comment so SQL Server can pick
/// add-vs-update extended-property procedures. Object rename runs last: once
/// the object moves, the old dotted path stops resolving. Returns a clear
/// per-change error when the engine cannot express one of the staged changes
/// — the review sheet shows the reason instead of firing another engine's
/// SQL.
pub fn ddl_plan_for(
    db: sqlly_lang::sqlmodel::DatabaseType,
    target: &EditorTarget,
    changes: &PendingChanges,
    current_no_recompute: bool,
    current_description: Option<&str>,
) -> Result<Vec<String>, String> {
    use sqlly_lang::sqlmodel::ConnectionKind;
    let kind = db.connection_kind();
    // Server setting edits plan independently of the object facets: a Server
    // target offers no rename/nullability facet and a table target can never
    // stage a setting, so the two never mix in one batch.
    if !changes.server_settings.is_empty() {
        return server_settings_plan(db, changes);
    }
    // Descriptions plan in the comment-capable engines' own forms; the
    // structural facets keep their narrower engine list.
    let structural = changes.rename_object.is_some()
        || !changes.rename_columns.is_empty()
        || !changes.column_nullability.is_empty()
        || changes.statistic_no_recompute.is_some()
        || changes.update_statistics_now;
    if structural
        && !matches!(
            kind,
            ConnectionKind::Tds
                | ConnectionKind::PostgresWire
                | ConnectionKind::MySqlWire
                | ConnectionKind::SqliteFile
                | ConnectionKind::HranaHttp
        )
    {
        return Err(crate::localization::format(
            "explorer.object_editor.changes_unsupported_engine",
            &[("engine", &db.display_name())],
        ));
    }
    let mut statements = Vec::new();
    let (schema, object) = target.query_object();
    let q = |name: &str| db.quote_identifier(name);
    // Schema-qualified table reference in the engine's own quoting; MySQL
    // and SQLite have no schema part in the tree's object addressing.
    let table_ref = match kind {
        ConnectionKind::Tds | ConnectionKind::PostgresWire => {
            format!("{}.{}", q(&schema), q(&object))
        }
        _ => q(&object),
    };

    if let Some(staged) = &changes.description {
        if !matches!(
            target.kind,
            EditorObjectKind::Table | EditorObjectKind::View
        ) {
            return Err(crate::localization::text(
                "explorer.object_editor.description_unsupported_kind",
            ));
        }
        let to = if staged.trim().is_empty() {
            None
        } else {
            Some(staged.clone())
        };
        let comment_kind = match target.kind {
            EditorObjectKind::View => CommentObjectKind::View,
            _ => CommentObjectKind::Table,
        };
        let change = CommentChange {
            column: None,
            from: current_description.map(str::to_string),
            to,
        };
        // The comment follows the tree's addressing convention: no schema
        // part for MySQL/SQLite, full qualification everywhere else.
        let comment_schema = match kind {
            ConnectionKind::Tds
            | ConnectionKind::PostgresWire
            | ConnectionKind::OracleNet
            | ConnectionKind::DuckDbFile
            | ConnectionKind::ClickHouseHttp => Some(schema.as_str()),
            _ => None,
        };
        match render_comment_change(db, comment_schema, &object, comment_kind, &change, None) {
            Ok(Some(sql)) => statements.push(sql.trim_end().to_string()),
            Ok(None) => {}
            Err(reason) => return Err(reason),
        }
    }

    for (column, new_name) in &changes.rename_columns {
        match kind {
            ConnectionKind::Tds => statements.push(format!(
                "EXEC sp_rename N'{}', N'{}', N'COLUMN';",
                escape_literal(&format!("{schema}.{object}.{column}")),
                escape_literal(new_name)
            )),
            // PostgreSQL, MySQL 8+, and SQLite share the standard form.
            _ => statements.push(format!(
                "ALTER TABLE {table_ref} RENAME COLUMN {} TO {};",
                q(column),
                q(new_name)
            )),
        }
    }
    for (column, change) in &changes.column_nullability {
        let nullability = if change.nullable { "NULL" } else { "NOT NULL" };
        match kind {
            ConnectionKind::Tds => statements.push(format!(
                "ALTER TABLE {}.{} ALTER COLUMN {} {} {};",
                quote_name(&schema),
                quote_name(&object),
                quote_name(column),
                change.alter_type,
                nullability
            )),
            ConnectionKind::PostgresWire => statements.push(format!(
                "ALTER TABLE {table_ref} ALTER COLUMN {} {} NOT NULL;",
                q(column),
                if change.nullable { "DROP" } else { "SET" }
            )),
            ConnectionKind::MySqlWire => statements.push(format!(
                "ALTER TABLE {table_ref} MODIFY {} {} {};",
                q(column),
                change.alter_type,
                nullability
            )),
            _ => {
                return Err(crate::localization::format(
                    "explorer.object_editor.nullability_unsupported_engine",
                    &[("engine", &db.display_name())],
                ));
            }
        }
    }
    if let Some(no_recompute) = changes.statistic_no_recompute {
        match kind {
            ConnectionKind::Tds => statements.push(update_statistics_sql(
                &schema,
                &object,
                &target.name,
                no_recompute,
            )),
            _ => {
                return Err(crate::localization::format(
                    "explorer.object_editor.statistics_unsupported_engine",
                    &[("engine", &db.display_name())],
                ));
            }
        }
    }
    if changes.update_statistics_now {
        match kind {
            ConnectionKind::Tds => {
                let effective = changes
                    .statistic_no_recompute
                    .unwrap_or(current_no_recompute);
                statements.push(update_statistics_sql(
                    &schema,
                    &object,
                    &target.name,
                    effective,
                ));
            }
            // The engines' own refresh-statistics forms.
            ConnectionKind::PostgresWire => statements.push(format!("ANALYZE {table_ref};")),
            ConnectionKind::MySqlWire => statements.push(format!("ANALYZE TABLE {table_ref};")),
            ConnectionKind::SqliteFile => statements.push(format!("ANALYZE {table_ref};")),
            // The libSQL server refuses ANALYZE; there is no statistics
            // refresh to script.
            ConnectionKind::HranaHttp => {}
            _ => unreachable!("gated above"),
        }
    }
    if let Some(new_name) = &changes.rename_object {
        match kind {
            ConnectionKind::Tds => statements.push(format!(
                "EXEC sp_rename N'{}', N'{}', N'{}';",
                escape_literal(&target.engine_path()),
                escape_literal(new_name),
                target.kind.rename_category()
            )),
            ConnectionKind::MySqlWire => {
                statements.push(format!("RENAME TABLE {table_ref} TO {};", q(new_name)))
            }
            _ => statements.push(format!(
                "ALTER TABLE {table_ref} RENAME TO {};",
                q(new_name)
            )),
        }
    }
    Ok(statements)
}

fn update_statistics_sql(schema: &str, table: &str, statistic: &str, no_recompute: bool) -> String {
    let mut sql = format!(
        "UPDATE STATISTICS {} {} {}",
        quote_name(schema),
        quote_name(table),
        quote_name(statistic)
    );
    if no_recompute {
        sql.push_str(" WITH NORECOMPUTE");
    }
    sql.push(';');
    sql
}

/// Whether the Configuration page offers its Edit facet at all on this
/// engine, and why not when it doesn't. Engines whose settings only change
/// per session or through files a DBA owns stay read-only with the reason —
/// the same honesty the capability matrix gives the explorer menus.
pub fn server_settings_edit_reason(db: sqlly_lang::sqlmodel::DatabaseType) -> Option<String> {
    use sqlly_lang::sqlmodel::ConnectionKind;
    let key = match db.connection_kind() {
        ConnectionKind::Tds | ConnectionKind::PostgresWire | ConnectionKind::MySqlWire => {
            return None;
        }
        ConnectionKind::ClickHouseHttp => "explorer.object_editor.settings_clickhouse",
        ConnectionKind::OracleNet => "explorer.object_editor.settings_oracle",
        ConnectionKind::DuckDbFile => "explorer.object_editor.settings_duckdb",
        ConnectionKind::SqliteFile => "explorer.object_editor.settings_sqlite",
        ConnectionKind::HranaHttp => "explorer.object_editor.settings_libsql",
        ConnectionKind::Resp => "explorer.object_editor.settings_redis",
        _ => "explorer.object_editor.settings_unsupported",
    };
    Some(crate::localization::text(key))
}

/// Why this connection cannot apply server-setting edits at all, whatever the
/// engine: a SELECT-only profile refuses every write; a rollback-wrapped
/// profile would run the statements inside a transaction, where `ALTER
/// SYSTEM`, `RECONFIGURE`, and `SET CLUSTER SETTING` are errors and MySQL's
/// `SET GLOBAL` takes effect despite the ROLLBACK (so the wrap would promise
/// an undo it cannot give); a locked production connection needs unlocking
/// first. `None` when the edit strip may stage and Apply may run.
pub fn server_settings_profile_block(
    block_all_but_select: bool,
    auto_wrap_rollback: bool,
    production_locked: bool,
) -> Option<String> {
    if block_all_but_select {
        Some(crate::localization::text(
            "explorer.object_editor.settings_select_only",
        ))
    } else if production_locked {
        Some(crate::localization::text(
            "explorer.object_editor.settings_production_locked",
        ))
    } else if auto_wrap_rollback {
        Some(crate::localization::text(
            "explorer.object_editor.settings_rollback_wrapped",
        ))
    } else {
        None
    }
}

/// The reviewed write script for the staged server settings, in the engine's
/// own form: `sp_configure` + `RECONFIGURE` on SQL Server (turning `show
/// advanced options` on around advanced options when it is off), `ALTER
/// SYSTEM` + `pg_reload_conf()` on PostgreSQL (`SET CLUSTER SETTING` on
/// CockroachDB), `SET GLOBAL`/`SET PERSIST` on the MySQL family. An empty
/// staged value resets to the server default where the engine has a form
/// for it. Engines without a runtime SQL write path get the reason instead.
fn server_settings_plan(
    db: sqlly_lang::sqlmodel::DatabaseType,
    changes: &PendingChanges,
) -> Result<Vec<String>, String> {
    use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};
    if let Some(reason) = server_settings_edit_reason(db) {
        return Err(reason.to_string());
    }
    let kind = db.connection_kind();
    if kind == ConnectionKind::Tds {
        return tsql_settings_plan(changes);
    }
    let mut statements = Vec::new();
    for (name, change) in &changes.server_settings {
        match kind {
            ConnectionKind::PostgresWire => {
                if db == DatabaseType::CockroachDb {
                    let name = bare_setting_name(name)?;
                    statements.push(if change.is_reset() {
                        format!("RESET CLUSTER SETTING {name};")
                    } else {
                        format!(
                            "SET CLUSTER SETTING {name} = {};",
                            pg_quote(change.value.trim())
                        )
                    });
                } else {
                    let quoted = db.quote_identifier(name);
                    statements.push(if change.is_reset() {
                        format!("ALTER SYSTEM RESET {quoted};")
                    } else {
                        format!(
                            "ALTER SYSTEM SET {quoted} = {};",
                            pg_setting_value(name, &change.value)
                        )
                    });
                }
            }
            ConnectionKind::MySqlWire => {
                let verb = if change.persist {
                    if db == DatabaseType::MySql {
                        "SET PERSIST"
                    } else {
                        return Err(
                            "SET PERSIST is MySQL 8 only — this engine supports SET GLOBAL (until restart)."
                                .to_string(),
                        );
                    }
                } else {
                    "SET GLOBAL"
                };
                let value = if change.is_reset() {
                    "DEFAULT".to_string()
                } else {
                    mysql_setting_value(&change.value)
                };
                statements.push(format!("{verb} {} = {value};", bare_setting_name(name)?));
            }
            _ => unreachable!("gated by server_settings_edit_reason"),
        }
    }
    if kind == ConnectionKind::PostgresWire && db != DatabaseType::CockroachDb {
        statements.push("SELECT pg_reload_conf();".to_string());
    }
    Ok(statements)
}

fn sp_configure(name: &str, value: &str) -> String {
    format!(
        "EXEC sys.sp_configure N'{}', {value};",
        escape_literal(name)
    )
}

/// SQL Server's settings script. Advanced options need `show advanced
/// options` in use first, so when it is off the script switches it on, makes
/// the edits, and switches it back off. A staged change to `show advanced
/// options` itself runs first when it turns the option on and last when it
/// turns it off, so the other edits always see it on.
fn tsql_settings_plan(changes: &PendingChanges) -> Result<Vec<String>, String> {
    let toggle = changes.server_settings_toggle_advanced();
    let mut statements = Vec::new();
    let mut show_advanced_last = None;
    if toggle {
        statements.push(sp_configure(SHOW_ADVANCED_OPTIONS, "1"));
        statements.push("RECONFIGURE;".to_string());
    }
    if let Some(change) = changes.server_settings.get(SHOW_ADVANCED_OPTIONS) {
        let value = tsql_setting_value(SHOW_ADVANCED_OPTIONS, change)?;
        if value == "0" {
            show_advanced_last = Some(value);
        } else {
            statements.push(sp_configure(SHOW_ADVANCED_OPTIONS, &value));
            statements.push("RECONFIGURE;".to_string());
        }
    }
    let mut any = false;
    for (name, change) in &changes.server_settings {
        if name == SHOW_ADVANCED_OPTIONS {
            continue;
        }
        statements.push(sp_configure(name, &tsql_setting_value(name, change)?));
        any = true;
    }
    if any {
        statements.push("RECONFIGURE;".to_string());
    }
    if let Some(value) = show_advanced_last {
        statements.push(sp_configure(SHOW_ADVANCED_OPTIONS, &value));
        statements.push("RECONFIGURE;".to_string());
    }
    statements.extend(server_settings_failure_cleanup(
        sqlly_lang::sqlmodel::DatabaseType::SqlServer,
        changes,
    ));
    Ok(statements)
}

/// The statements that put `show advanced options` back off when a settings
/// script that switched it on stops part-way (the reviewed script's own
/// final two statements). Empty when the script never touched it.
pub fn server_settings_failure_cleanup(
    db: sqlly_lang::sqlmodel::DatabaseType,
    changes: &PendingChanges,
) -> Vec<String> {
    use sqlly_lang::sqlmodel::ConnectionKind;
    if db.connection_kind() == ConnectionKind::Tds && changes.server_settings_toggle_advanced() {
        vec![
            sp_configure(SHOW_ADVANCED_OPTIONS, "0"),
            "RECONFIGURE;".to_string(),
        ]
    } else {
        Vec::new()
    }
}

/// A setting name safe to emit bare (MySQL `SET GLOBAL`, CockroachDB `SET
/// CLUSTER SETTING`): letters, digits, `_`, and the dots of namespaced names
/// (`validate_password.policy`). Anything else is rejected rather than
/// interpolated.
fn bare_setting_name(name: &str) -> Result<&str, String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
    {
        return Err(crate::localization::format(
            "explorer.object_editor.setting_name_invalid",
            &[("name", &format!("{name:?}"))],
        ));
    }
    Ok(name)
}

/// T-SQL `sp_configure` value. Every `sys.configurations` option is a whole
/// number, so anything else is refused with the reason rather than sent as a
/// string the procedure would reject; SQL Server has no reset-to-default.
fn tsql_setting_value(name: &str, change: &ServerSettingChange) -> Result<String, String> {
    let value = change.value.trim();
    if change.is_reset() {
        return Err(crate::localization::format(
            "explorer.object_editor.sqlserver_reset_unsupported",
            &[("name", name)],
        ));
    }
    value.parse::<i64>().map(|n| n.to_string()).map_err(|_| {
        crate::localization::format(
            "explorer.object_editor.sqlserver_number_required",
            &[("name", name), ("value", &format!("{value:?}"))],
        )
    })
}

/// A single-quoted PostgreSQL string literal.
fn pg_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// The PostgreSQL settings whose value is a list of identifiers the server
/// quotes item by item (`GUC_LIST_QUOTE`): one quoted literal would become a
/// single list element, so `search_path = 'a, b'` means the schema "a, b".
const PG_LIST_QUOTE_SETTINGS: [&str; 6] = [
    "search_path",
    "temp_tablespaces",
    "shared_preload_libraries",
    "session_preload_libraries",
    "local_preload_libraries",
    "unix_socket_directories",
];

/// Split a displayed list value (`"$user", public`) into its items: commas
/// outside double quotes separate; a double-quoted item is unwrapped (its
/// `""` unescaped), because the server re-quotes each item itself.
fn pg_list_items(value: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                current.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => items.push(std::mem::take(&mut current).trim().to_string()),
            _ => current.push(c),
        }
    }
    items.push(current.trim().to_string());
    items.into_iter().filter(|item| !item.is_empty()).collect()
}

/// PostgreSQL `ALTER SYSTEM` value: a single-quoted literal the parser
/// coerces to the setting's type (the page shows `current_setting` output,
/// which is valid input, units included), or — for list settings — one
/// literal per item.
fn pg_setting_value(name: &str, value: &str) -> String {
    let value = value.trim();
    if PG_LIST_QUOTE_SETTINGS
        .iter()
        .any(|list| list.eq_ignore_ascii_case(name))
    {
        let items = pg_list_items(value);
        if !items.is_empty() {
            return items
                .iter()
                .map(|item| pg_quote(item))
                .collect::<Vec<_>>()
                .join(", ");
        }
    }
    pg_quote(value)
}

/// Whether `text` is a plain finite decimal number (`-12`, `0.5`, `1e3`) —
/// Rust's float parser alone would also accept `inf` and `NaN`, which MySQL
/// would then read as column names.
fn is_plain_number(text: &str) -> bool {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    digits.starts_with(|c: char| c.is_ascii_digit())
        && digits
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '-' | '+'))
        && text.parse::<f64>().is_ok_and(f64::is_finite)
}

/// MySQL `SET GLOBAL` value: numbers and the boolean keywords bare, strings
/// quoted with MySQL's backslash-and-quote doubling.
fn mysql_setting_value(value: &str) -> String {
    let trimmed = value.trim();
    if is_plain_number(trimmed)
        || matches!(
            trimmed.to_ascii_uppercase().as_str(),
            "ON" | "OFF" | "TRUE" | "FALSE"
        )
    {
        trimmed.to_string()
    } else {
        format!("'{}'", trimmed.replace('\\', "\\\\").replace('\'', "''"))
    }
}

fn cell_str(cell: Option<&CellValue>) -> Option<String> {
    match cell? {
        CellValue::Null => None,
        CellValue::Bool(b) => Some(b.to_string()),
        CellValue::I64(n) => Some(n.to_string()),
        CellValue::F64(n) => Some(n.to_string()),
        CellValue::String(s) => Some(s.clone()),
        CellValue::Bytes(_) => None,
    }
}

fn cell_i64(cell: Option<&CellValue>) -> Option<i64> {
    match cell? {
        CellValue::Null => None,
        CellValue::I64(n) => Some(*n),
        CellValue::F64(n) => Some(*n as i64),
        CellValue::Bool(b) => Some(*b as i64),
        CellValue::String(s) => s.parse().ok(),
        CellValue::Bytes(_) => None,
    }
}

fn cell_bool(cell: Option<&CellValue>) -> Option<bool> {
    match cell? {
        CellValue::Null => None,
        CellValue::Bool(b) => Some(*b),
        CellValue::I64(n) => Some(*n != 0),
        CellValue::F64(n) => Some(*n != 0.0),
        CellValue::String(s) => Some(s.eq_ignore_ascii_case("true") || s == "1"),
        CellValue::Bytes(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_lang::sqlmodel::DatabaseType;

    fn table_target() -> EditorTarget {
        EditorTarget::new(EditorObjectKind::Table, "Sales", "dbo", "Orders")
    }

    /// A server target: no schema, no database of its own, named for the
    /// connection it was opened from.
    fn server_target() -> EditorTarget {
        EditorTarget::new(EditorObjectKind::Server, "", "", "pg-primary")
    }

    fn database_target() -> EditorTarget {
        EditorTarget::new(EditorObjectKind::Database, "Sales", "", "Sales")
    }

    fn schema_target() -> EditorTarget {
        EditorTarget::new(EditorObjectKind::Schema, "Sales", "", "reporting")
    }

    fn index_target() -> EditorTarget {
        EditorTarget::new(EditorObjectKind::Index, "Sales", "dbo", "IX_Orders_Date")
            .child_of("Orders")
    }

    fn constraint_target() -> EditorTarget {
        EditorTarget::new(
            EditorObjectKind::Constraint,
            "Sales",
            "dbo",
            "CK_Orders_Total",
        )
        .child_of("Orders")
    }

    #[test]
    fn pages_match_kind() {
        let table_pages: Vec<EditorPage> = pages_for(EditorObjectKind::Table)
            .iter()
            .map(|spec| spec.page)
            .collect();
        assert_eq!(
            table_pages,
            vec![
                EditorPage::General,
                EditorPage::Columns,
                EditorPage::Indexes,
                EditorPage::Statistics,
                EditorPage::Constraints,
                EditorPage::Triggers,
                EditorPage::Permissions
            ]
        );
        let index_pages: Vec<EditorPage> = pages_for(EditorObjectKind::Index)
            .iter()
            .map(|spec| spec.page)
            .collect();
        assert_eq!(index_pages, vec![EditorPage::General, EditorPage::Columns]);
        assert_eq!(
            pages_for(EditorObjectKind::Server)
                .iter()
                .map(|spec| spec.page)
                .collect::<Vec<_>>(),
            vec![
                EditorPage::General,
                EditorPage::Databases,
                EditorPage::Configuration,
                EditorPage::Status,
                EditorPage::Permissions
            ]
        );
        assert_eq!(
            pages_for(EditorObjectKind::Database)
                .iter()
                .map(|spec| spec.page)
                .collect::<Vec<_>>(),
            vec![
                EditorPage::General,
                EditorPage::Files,
                EditorPage::Objects,
                EditorPage::Permissions
            ]
        );
        assert_eq!(
            pages_for(EditorObjectKind::Schema)
                .iter()
                .map(|spec| spec.page)
                .collect::<Vec<_>>(),
            vec![EditorPage::General, EditorPage::Objects]
        );
        assert_eq!(
            pages_for(EditorObjectKind::Constraint)
                .iter()
                .map(|spec| spec.page)
                .collect::<Vec<_>>(),
            vec![EditorPage::General, EditorPage::Definition]
        );
        assert_eq!(
            pages_for(EditorObjectKind::Trigger)
                .iter()
                .map(|spec| spec.page)
                .collect::<Vec<_>>(),
            vec![
                EditorPage::General,
                EditorPage::Definition,
                EditorPage::Permissions
            ]
        );
        assert_eq!(
            pages_for(EditorObjectKind::Type).len(),
            1,
            "types are read-only single-page dialogs"
        );
    }

    #[test]
    fn markers_match_tree_dialect() {
        assert_eq!(EditorObjectKind::Table.marker(), "T");
        assert_eq!(EditorObjectKind::Index.marker(), "IX");
        assert_eq!(EditorObjectKind::Statistic.marker(), "ST");
        assert_eq!(EditorObjectKind::Constraint.marker(), "CK");
        assert_eq!(EditorObjectKind::Trigger.marker(), "TR");
        assert_eq!(EditorObjectKind::Type.marker(), "TY");
    }

    #[test]
    fn index_query_keys_on_parent_table() {
        let query = query_for_page(&index_target(), EditorPage::General);
        match query {
            MetadataQuery::IndexGeneral {
                schema,
                table,
                index,
            } => {
                assert_eq!(schema, "dbo");
                assert_eq!(table, "Orders");
                assert_eq!(index, "IX_Orders_Date");
            }
            other => panic!("expected IndexGeneral, got {other:?}"),
        }
    }

    #[test]
    fn constraint_definition_uses_parent_table_and_own_name() {
        let query = query_for_page(&constraint_target(), EditorPage::Definition);
        match query {
            MetadataQuery::ConstraintDefinition {
                schema,
                table,
                name,
            } => {
                assert_eq!(schema, "dbo");
                assert_eq!(table, "Orders");
                assert_eq!(name, "CK_Orders_Total");
            }
            other => panic!("expected ConstraintDefinition, got {other:?}"),
        }
    }

    #[test]
    fn trigger_uses_its_own_name_for_general_metadata() {
        let target = EditorTarget::new(EditorObjectKind::Trigger, "Sales", "dbo", "tr_Orders")
            .child_of("Orders");
        let query = query_for_page(&target, EditorPage::General);
        match query {
            MetadataQuery::ObjectGeneral { schema, object } => {
                assert_eq!(schema, "dbo");
                assert_eq!(object, "tr_Orders");
            }
            other => panic!("expected ObjectGeneral, got {other:?}"),
        }
    }

    #[test]
    fn qualified_names_cover_child_objects() {
        let target = index_target();
        assert_eq!(target.qualified_name(), "dbo.IX_Orders_Date");
        assert_eq!(target.parent_context().as_deref(), Some("dbo.Orders"));
        assert!(table_target().parent_context().is_none());
    }

    #[test]
    fn display_type_handles_unicode_lengths_and_max() {
        assert_eq!(
            display_type("nvarchar", Some(100), None, None),
            "nvarchar(50)"
        );
        assert_eq!(
            display_type("nvarchar", Some(-1), None, None),
            "nvarchar(max)"
        );
        assert_eq!(display_type("varchar", Some(50), None, None), "varchar(50)");
        assert_eq!(
            display_type("decimal", None, Some(18), Some(2)),
            "decimal(18,2)"
        );
        assert_eq!(display_type("int", None, None, None), "int");
    }

    #[test]
    fn alter_type_brackets_and_decorates() {
        assert_eq!(
            alter_type("nvarchar", Some(100), None, None),
            "[nvarchar](50)"
        );
        assert_eq!(
            alter_type("decimal", None, Some(9), Some(3)),
            "[decimal](9,3)"
        );
        assert_eq!(alter_type("int", None, None, None), "[int]");
    }

    #[test]
    fn parses_table_general_row() {
        let rows = vec![vec![
            CellValue::String("dbo".into()),
            CellValue::String("Orders".into()),
            CellValue::String("2026-01-01".into()),
            CellValue::String("2026-07-01".into()),
            CellValue::I64(1523),
        ]];
        let PageData::Properties(props) =
            parse_page_data(&table_target(), EditorPage::General, &rows)
        else {
            panic!("expected properties");
        };
        assert!(
            props
                .iter()
                .any(|p| p.label == "Rows (estimated)" && p.value == "1523")
        );
    }

    #[test]
    fn parses_the_database_general_row() {
        // The query now asks for one database's own row, so there is exactly
        // one; every engine returns all eight columns.
        let rows = vec![vec![
            CellValue::String("Sales".into()),
            CellValue::String("ONLINE".into()),
            CellValue::String("SQL_Latin1_General_CP1_CI_AS".into()),
            CellValue::String("FULL".into()),
            CellValue::I64(160),
            CellValue::String("2024-01-05 09:12:44".into()),
            CellValue::String("4096.00".into()),
            CellValue::String("sa".into()),
        ]];
        let PageData::Properties(props) =
            parse_page_data(&database_target(), EditorPage::General, &rows)
        else {
            panic!("expected properties");
        };
        assert_eq!(
            props,
            vec![
                property("Name", "Sales"),
                property("State", "ONLINE"),
                property("Collation", "SQL_Latin1_General_CP1_CI_AS"),
                property("Recovery model", "FULL"),
                property("Compatibility level", "160"),
                property("Created", "2024-01-05 09:12:44"),
                property("Size", "4096.00 MB"),
                property("Owner", "sa"),
            ]
        );
    }

    /// The server pages are server-scope: none of them keys on the target's
    /// (empty) schema or object name, and the database scoping stays on the
    /// database pages.
    #[test]
    fn server_and_database_pages_use_scope_queries() {
        let server = server_target();
        assert_eq!(
            query_for_page(&server, EditorPage::General),
            MetadataQuery::ServerCapability
        );
        assert_eq!(
            query_for_page(&server, EditorPage::Databases),
            MetadataQuery::Databases
        );
        assert_eq!(
            query_for_page(&server, EditorPage::Configuration),
            MetadataQuery::ServerConfiguration
        );
        assert_eq!(
            query_for_page(&server, EditorPage::Status),
            MetadataQuery::ServerStatus
        );
        assert_eq!(
            query_for_page(&server, EditorPage::Permissions),
            MetadataQuery::ServerPermissions
        );

        let database = database_target();
        assert_eq!(
            query_for_page(&database, EditorPage::Files),
            MetadataQuery::DatabaseFiles {
                database: "Sales".into()
            }
        );
        assert_eq!(
            query_for_page(&database, EditorPage::Permissions),
            MetadataQuery::DatabasePermissions {
                database: "Sales".into()
            }
        );
        assert_eq!(
            query_for_page(&database, EditorPage::Objects),
            MetadataQuery::ObjectInventory { schema: None },
            "a database's Objects page spans every schema"
        );
    }

    #[test]
    fn parses_the_server_general_row() {
        let rows = vec![vec![
            CellValue::String("16.0.4145.4".into()),
            CellValue::String("Developer Edition (64-bit)".into()),
            CellValue::I64(3),
            CellValue::String("RTM".into()),
            CellValue::String("CU12".into()),
            CellValue::String("SQL01".into()),
            CellValue::I64(0),
        ]];
        let PageData::Properties(props) =
            parse_page_data(&server_target(), EditorPage::General, &rows)
        else {
            panic!("expected properties");
        };
        assert_eq!(
            props,
            vec![
                property("Machine name", "SQL01"),
                property("Product version", "16.0.4145.4"),
                property("Edition", "Developer Edition (64-bit)"),
                property("Engine edition", "3"),
                property("Product level", "RTM"),
                property("Update level", "CU12"),
                property("Authentication", "SQL and integrated security"),
            ]
        );
    }

    /// Engines that report no machine name or auth mode fall back to the
    /// connection's own name and say what is missing, keeping one page shape.
    #[test]
    fn server_general_falls_back_to_the_connection_name() {
        let rows = vec![vec![
            CellValue::String("16.2".into()),
            CellValue::String("PostgreSQL".into()),
            CellValue::Null,
            CellValue::Null,
            CellValue::Null,
            CellValue::Null,
            CellValue::I64(0),
        ]];
        let PageData::Properties(props) =
            parse_page_data(&server_target(), EditorPage::General, &rows)
        else {
            panic!("expected properties");
        };
        assert_eq!(props[0], property("Machine name", "pg-primary"));
        assert_eq!(props[3], property("Engine edition", "not reported"));
    }

    #[test]
    fn parses_the_databases_page() {
        let rows = vec![
            vec![
                CellValue::String("Sales".into()),
                CellValue::String("ONLINE".into()),
                CellValue::I64(160),
            ],
            vec![
                CellValue::String("chinook".into()),
                CellValue::String("ONLINE".into()),
                CellValue::Null,
            ],
        ];
        let PageData::Databases(rows) =
            parse_page_data(&server_target(), EditorPage::Databases, &rows)
        else {
            panic!("expected databases");
        };
        assert_eq!(rows[0].grid_row(), vec!["Sales", "ONLINE", "160"]);
        assert_eq!(
            rows[1].grid_row(),
            vec!["chinook", "ONLINE", ""],
            "an engine without compatibility levels leaves the cell blank"
        );
    }

    /// A NULL max size means "unlimited" where the engine reports files at
    /// all, and nothing where it reports no size either.
    #[test]
    fn file_rows_spell_out_an_absent_maximum() {
        let rows = vec![
            vec![
                CellValue::String("Sales_Data".into()),
                CellValue::String("ROWS".into()),
                CellValue::String("/var/opt/mssql/data/Sales.mdf".into()),
                CellValue::String("4096.0".into()),
                CellValue::Null,
                CellValue::String("64.0 MB".into()),
            ],
            vec![
                CellValue::String("chinook".into()),
                CellValue::String("DATABASE".into()),
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
            ],
        ];
        let PageData::Files(files) = parse_page_data(&database_target(), EditorPage::Files, &rows)
        else {
            panic!("expected files");
        };
        assert_eq!(
            files[0].grid_row(),
            vec![
                "Sales_Data",
                "ROWS",
                "/var/opt/mssql/data/Sales.mdf",
                "4096.0",
                "unlimited",
                "64.0 MB"
            ]
        );
        assert_eq!(
            files[1].grid_row(),
            vec!["chinook", "DATABASE", "", "", "", ""]
        );
    }

    #[test]
    fn parses_the_configuration_page() {
        let rows = vec![vec![
            CellValue::String("max server memory (MB)".into()),
            CellValue::I64(2147483647),
            CellValue::String("Maximum size of server memory pool".into()),
            CellValue::I64(1),
        ]];
        let PageData::Settings(settings) =
            parse_page_data(&server_target(), EditorPage::Configuration, &rows)
        else {
            panic!("expected settings");
        };
        assert_eq!(
            settings[0].grid_row(),
            vec![
                "max server memory (MB)",
                "2147483647",
                "Maximum size of server memory pool"
            ]
        );
        assert!(settings[0].is_dynamic);
        assert!(!settings[0].advanced, "a four-column row is not advanced");
        // SQL Server's fifth column marks advanced options.
        let advanced = vec![vec![
            CellValue::String("max degree of parallelism".into()),
            CellValue::String("0".into()),
            CellValue::String("maximum degree of parallelism".into()),
            CellValue::Bool(true),
            CellValue::Bool(true),
        ]];
        let PageData::Settings(settings) =
            parse_page_data(&server_target(), EditorPage::Configuration, &advanced)
        else {
            panic!("expected settings");
        };
        assert!(settings[0].advanced);
        // An engine that omits the flag reports read-only, never editable.
        let three_col = vec![vec![
            CellValue::String("work_mem".into()),
            CellValue::String("4MB".into()),
            CellValue::String("default".into()),
        ]];
        let PageData::Settings(settings) =
            parse_page_data(&server_target(), EditorPage::Configuration, &three_col)
        else {
            panic!("expected settings");
        };
        assert!(!settings[0].is_dynamic);
    }

    #[test]
    fn redis_info_rows_reshape_into_status_counters() {
        // The query path shapes INFO as (section, key, value); the page wants
        // (name, value, source).
        let rows = vec![vec![
            CellValue::String("Stats".into()),
            CellValue::String("total_commands_processed".into()),
            CellValue::String("98765".into()),
        ]];
        let reshaped = redis_status_rows(&rows);
        let PageData::Settings(counters) =
            parse_page_data(&server_target(), EditorPage::Status, &reshaped)
        else {
            panic!("expected settings");
        };
        assert_eq!(
            counters[0].grid_row(),
            vec!["total_commands_processed", "98765", "Stats"]
        );
        assert!(redis_status_rows(&[]).is_empty());
    }

    #[test]
    fn parses_the_status_page_as_read_only_counters() {
        let rows = vec![vec![
            CellValue::String("xact_committed".into()),
            CellValue::String("123456".into()),
            CellValue::String("pg_stat_database".into()),
        ]];
        let PageData::Settings(counters) =
            parse_page_data(&server_target(), EditorPage::Status, &rows)
        else {
            panic!("expected settings");
        };
        assert_eq!(
            counters[0].grid_row(),
            vec!["xact_committed", "123456", "pg_stat_database"]
        );
        assert!(
            !counters[0].is_dynamic,
            "status counters are never editable"
        );
    }

    #[test]
    fn filter_settings_matches_name_value_or_source_case_insensitively() {
        let settings = vec![
            SettingInfo {
                name: "max_connections".into(),
                value: "151".into(),
                source: Some("GLOBAL".into()),
                is_dynamic: true,
                advanced: false,
            },
            SettingInfo {
                name: "Innodb_buffer_pool_reads".into(),
                value: "42".into(),
                source: Some("STATUS".into()),
                is_dynamic: false,
                advanced: false,
            },
            SettingInfo {
                name: "work_mem".into(),
                value: "4MB".into(),
                source: None,
                is_dynamic: true,
                advanced: false,
            },
        ];
        assert_eq!(filter_settings(&settings, "  ").len(), 3);
        let by_name = filter_settings(&settings, "BUFFER");
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].name, "Innodb_buffer_pool_reads");
        assert_eq!(filter_settings(&settings, "4mb")[0].name, "work_mem");
        assert_eq!(
            filter_settings(&settings, "global")[0].name,
            "max_connections"
        );
        assert!(filter_settings(&settings, "nothing-like-this").is_empty());
    }

    #[test]
    fn status_page_headers_name_counters() {
        assert_eq!(
            settings_headers(EditorPage::Status),
            vec!["Counter", "Value", "Class"]
        );
        assert_eq!(
            settings_headers(EditorPage::Configuration),
            vec!["Setting", "Value", "Source"]
        );
    }

    #[test]
    fn status_empty_copy_is_honest_per_engine() {
        use sqlly_lang::sqlmodel::DatabaseType;
        assert!(status_empty_copy(DatabaseType::Sqlite).contains("in-process"));
        assert!(status_empty_copy(DatabaseType::DuckDb).contains("no server status counters"));
        assert!(status_empty_copy(DatabaseType::LibSql).contains("no server status counters"));
        assert!(status_empty_copy(DatabaseType::CockroachDb).contains("crdb_internal"));
        assert!(status_empty_copy(DatabaseType::Postgres).contains("reported no status"));
    }

    #[test]
    fn staging_a_server_setting_tracks_revert_and_clears_on_the_original() {
        let mut changes = PendingChanges::default();
        changes.stage_server_setting("work_mem", "4MB", "64MB", false);
        assert_eq!(changes.pending_count(), 1);
        assert_eq!(
            changes.server_settings["work_mem"].revert, "4MB",
            "the revert value is what the engine reported"
        );
        changes.stage_server_setting("work_mem", "4MB", "4MB", false);
        assert!(!changes.is_dirty(), "staging the live value un-stages");
    }

    #[test]
    fn setting_summary_names_old_and_new_and_notes_the_revert_path() {
        let mut changes = PendingChanges::default();
        changes.stage_server_setting("work_mem", "4MB", "64MB", false);
        let lines = changes.summary_lines(EditorObjectKind::Server);
        assert!(
            lines.iter().any(|l| l == "Set work_mem to 64MB (was 4MB)"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("not transactional")),
            "engines without rollback get the note: {lines:?}"
        );
    }

    #[test]
    fn ddl_plan_for_postgres_settings_uses_alter_system_and_reload() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let mut changes = PendingChanges::default();
        changes.stage_server_setting("work_mem", "4MB", "64MB", false);
        changes.stage_server_setting("log_min_duration_statement", "-1", "it's 500", false);
        let plan = ddl_plan_for(
            DatabaseType::Postgres,
            &server_target(),
            &changes,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            plan,
            vec![
                "ALTER SYSTEM SET \"log_min_duration_statement\" = 'it''s 500';".to_string(),
                "ALTER SYSTEM SET \"work_mem\" = '64MB';".to_string(),
                "SELECT pg_reload_conf();".to_string(),
            ]
        );
    }

    #[test]
    fn ddl_plan_for_cockroach_settings_uses_cluster_setting() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let mut changes = PendingChanges::default();
        changes.stage_server_setting("sql.defaults.default_int_size", "8", "4", false);
        let plan = ddl_plan_for(
            DatabaseType::CockroachDb,
            &server_target(),
            &changes,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            plan,
            vec!["SET CLUSTER SETTING sql.defaults.default_int_size = '4';"]
        );
    }

    #[test]
    fn ddl_plan_for_mysql_settings_types_values_and_persists() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let mut changes = PendingChanges::default();
        changes.stage_server_setting("max_connections", "151", "500", true);
        changes.stage_server_setting("event_scheduler", "OFF", "ON", false);
        changes.stage_server_setting("slow_query_log_file", "/a/b", "C:\\log\\it's.sql", false);
        let plan =
            ddl_plan_for(DatabaseType::MySql, &server_target(), &changes, false, None).unwrap();
        assert_eq!(
            plan,
            vec![
                "SET GLOBAL event_scheduler = ON;".to_string(),
                "SET PERSIST max_connections = 500;".to_string(),
                "SET GLOBAL slow_query_log_file = 'C:\\\\log\\\\it''s.sql';".to_string(),
            ]
        );
        // MariaDB has no SET PERSIST: asking for it fails with the reason.
        let err = ddl_plan_for(
            DatabaseType::MariaDb,
            &server_target(),
            &changes,
            false,
            None,
        )
        .expect_err("persist on mariadb");
        assert!(err.contains("SET PERSIST"), "{err}");
        // Without the flag MariaDB scripts SET GLOBAL fine.
        let mut maria = PendingChanges::default();
        maria.stage_server_setting("max_connections", "151", "500", false);
        assert_eq!(
            ddl_plan_for(DatabaseType::MariaDb, &server_target(), &maria, false, None).unwrap(),
            vec!["SET GLOBAL max_connections = 500;"]
        );
    }

    #[test]
    fn ddl_plan_for_sql_server_settings_uses_sp_configure_and_reconfigure() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let mut changes = PendingChanges::default();
        changes.stage_server_setting("max server memory (MB)", "2147483647", "8192", false);
        changes.stage_server_setting("default language", "0", "2", false);
        let plan = ddl_plan_for(
            DatabaseType::SqlServer,
            &server_target(),
            &changes,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            plan,
            vec![
                "EXEC sys.sp_configure N'default language', 2;".to_string(),
                "EXEC sys.sp_configure N'max server memory (MB)', 8192;".to_string(),
                "RECONFIGURE;".to_string(),
            ]
        );
    }

    /// Every sp_configure option is a whole number (`default language` is a
    /// langid), and SQL Server has no reset-to-default statement: both are
    /// refused with the reason instead of sending a string the procedure
    /// rejects.
    #[test]
    fn sql_server_settings_refuse_non_integers_and_resets() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let mut changes = PendingChanges::default();
        changes.stage_server_setting("default language", "0", "français", false);
        let err = ddl_plan_for(
            DatabaseType::SqlServer,
            &server_target(),
            &changes,
            false,
            None,
        )
        .expect_err("text value");
        assert!(err.contains("whole number"), "{err}");
        let mut reset = PendingChanges::default();
        reset.stage_server_setting("cost threshold for parallelism", "5", "", false);
        let err = ddl_plan_for(
            DatabaseType::SqlServer,
            &server_target(),
            &reset,
            false,
            None,
        )
        .expect_err("reset");
        assert!(err.contains("no reset-to-default"), "{err}");
    }

    /// Advanced options fail with error 15123 while `show advanced options`
    /// is off: the script turns it on first, back off last, and the review
    /// summary says so.
    #[test]
    fn sql_server_advanced_options_are_wrapped_in_show_advanced_options() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let mut changes = PendingChanges::default();
        changes.stage_server_setting("max degree of parallelism", "0", "4", false);
        changes.mark_server_setting_advanced("max degree of parallelism", true);
        let plan = ddl_plan_for(
            DatabaseType::SqlServer,
            &server_target(),
            &changes,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            plan,
            vec![
                "EXEC sys.sp_configure N'show advanced options', 1;".to_string(),
                "RECONFIGURE;".to_string(),
                "EXEC sys.sp_configure N'max degree of parallelism', 4;".to_string(),
                "RECONFIGURE;".to_string(),
                "EXEC sys.sp_configure N'show advanced options', 0;".to_string(),
                "RECONFIGURE;".to_string(),
            ]
        );
        assert_eq!(
            server_settings_failure_cleanup(DatabaseType::SqlServer, &changes),
            plan[4..].to_vec(),
            "a failed run restores the option with the script's own tail"
        );
        let summary = changes.summary_lines(EditorObjectKind::Server);
        assert!(
            summary.iter().any(|l| l.contains("show advanced options")),
            "{summary:?}"
        );
        // Re-staging a new value keeps the advanced flag.
        changes.stage_server_setting("max degree of parallelism", "0", "8", false);
        assert!(changes.server_settings_toggle_advanced());

        // The user turning it on themselves: it runs first and is not undone.
        changes.stage_server_setting("show advanced options", "0", "1", false);
        let plan = ddl_plan_for(
            DatabaseType::SqlServer,
            &server_target(),
            &changes,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            plan.first().map(String::as_str),
            Some("EXEC sys.sp_configure N'show advanced options', 1;")
        );
        assert_eq!(
            plan.iter()
                .filter(|s| s.contains("show advanced options"))
                .count(),
            1
        );
        assert!(server_settings_failure_cleanup(DatabaseType::SqlServer, &changes).is_empty());

        // Non-advanced edits are never wrapped.
        let mut plain = PendingChanges::default();
        plain.stage_server_setting("remote access", "1", "0", false);
        let plan = ddl_plan_for(
            DatabaseType::SqlServer,
            &server_target(),
            &plain,
            false,
            None,
        )
        .unwrap();
        assert!(!plan.iter().any(|s| s.contains("show advanced options")));
    }

    /// List settings quote each item (one literal would be a single schema
    /// named "a, b"); a cleared input resets to the default.
    #[test]
    fn postgres_list_settings_quote_items_and_empty_resets() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let mut changes = PendingChanges::default();
        changes.stage_server_setting(
            "search_path",
            "\"$user\", public",
            "\"$user\", public, \"Odd, Name\"",
            false,
        );
        changes.stage_server_setting("shared_buffers", "128MB", "256MB", false);
        changes.stage_server_setting("work_mem", "4MB", "", false);
        let plan = ddl_plan_for(
            DatabaseType::Postgres,
            &server_target(),
            &changes,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            plan,
            vec![
                "ALTER SYSTEM SET \"search_path\" = '$user', 'public', 'Odd, Name';".to_string(),
                "ALTER SYSTEM SET \"shared_buffers\" = '256MB';".to_string(),
                "ALTER SYSTEM RESET \"work_mem\";".to_string(),
                "SELECT pg_reload_conf();".to_string(),
            ]
        );
        let summary = changes.summary_lines(EditorObjectKind::Server);
        assert!(
            summary
                .iter()
                .any(|l| l == "Reset work_mem to the server default (was 4MB)"),
            "{summary:?}"
        );
        // Non-list settings keep a single literal even with commas.
        assert_eq!(
            pg_setting_value("log_destination", "stderr, csvlog"),
            "'stderr, csvlog'"
        );
        let mut crdb = PendingChanges::default();
        crdb.stage_server_setting("sql.defaults.default_int_size", "8", "", false);
        assert_eq!(
            ddl_plan_for(
                DatabaseType::CockroachDb,
                &server_target(),
                &crdb,
                false,
                None
            )
            .unwrap(),
            vec!["RESET CLUSTER SETTING sql.defaults.default_int_size;"]
        );
    }

    #[test]
    fn mysql_values_reject_non_finite_numbers_and_reset_to_default() {
        use sqlly_lang::sqlmodel::DatabaseType;
        assert_eq!(mysql_setting_value("inf"), "'inf'");
        assert_eq!(mysql_setting_value("NaN"), "'NaN'");
        assert_eq!(mysql_setting_value("1e400"), "'1e400'");
        assert_eq!(mysql_setting_value("-12"), "-12");
        assert_eq!(mysql_setting_value("0.5"), "0.5");
        let mut changes = PendingChanges::default();
        changes.stage_server_setting("max_connections", "151", "", true);
        assert_eq!(
            ddl_plan_for(DatabaseType::MySql, &server_target(), &changes, false, None).unwrap(),
            vec!["SET PERSIST max_connections = DEFAULT;"]
        );
    }

    #[test]
    fn server_settings_profile_block_names_the_first_blocker() {
        assert_eq!(server_settings_profile_block(false, false, false), None);
        assert!(
            server_settings_profile_block(true, true, true)
                .unwrap()
                .contains("SELECT-only")
        );
        assert!(
            server_settings_profile_block(false, true, true)
                .unwrap()
                .contains("production")
        );
        assert!(
            server_settings_profile_block(false, true, false)
                .unwrap()
                .contains("rollback")
        );
    }

    #[test]
    fn ddl_plan_for_settings_refuses_unsafe_names_and_read_only_engines() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let mut bad = PendingChanges::default();
        bad.stage_server_setting("x'; DROP TABLE t--", "0", "1", false);
        let err = ddl_plan_for(DatabaseType::MySql, &server_target(), &bad, false, None)
            .expect_err("unsafe name");
        assert!(err.contains("cannot be scripted safely"), "{err}");
        for db in [
            DatabaseType::ClickHouse,
            DatabaseType::Oracle,
            DatabaseType::DuckDb,
            DatabaseType::Sqlite,
            DatabaseType::LibSql,
            DatabaseType::Redis,
        ] {
            let mut changes = PendingChanges::default();
            changes.stage_server_setting("anything", "0", "1", false);
            let err = ddl_plan_for(db, &server_target(), &changes, false, None)
                .expect_err("read-only engine must refuse");
            assert!(!err.is_empty(), "{db:?}");
        }
    }

    /// Engines without a recovery model or an on-disk size say so rather than
    /// dropping the line, so the page reads the same everywhere.
    #[test]
    fn database_general_reports_what_an_engine_does_not_have() {
        let rows = vec![vec![
            CellValue::String("chinook".into()),
            CellValue::String("ONLINE".into()),
            CellValue::String("en_US.UTF-8".into()),
            CellValue::Null,
            CellValue::Null,
            CellValue::Null,
            CellValue::String("12.50".into()),
            CellValue::String("postgres".into()),
        ]];
        let PageData::Properties(props) =
            parse_page_data(&database_target(), EditorPage::General, &rows)
        else {
            panic!("expected properties");
        };
        assert_eq!(
            props,
            vec![
                property("Name", "chinook"),
                property("State", "ONLINE"),
                property("Collation", "en_US.UTF-8"),
                property("Recovery model", "not reported"),
                property("Compatibility level", "not reported"),
                property("Created", "not reported"),
                property("Size", "12.50 MB"),
                property("Owner", "postgres"),
            ]
        );
    }

    /// A column's General page: the type line is assembled from the engine's
    /// separate type/length/precision columns, and the optional lines only
    /// appear when the column has them.
    #[test]
    fn parses_the_column_general_row() {
        let target = EditorTarget::new(EditorObjectKind::Column, "Sales", "dbo", "Email")
            .child_of("Customer".to_string());
        let rows = vec![vec![
            CellValue::String("dbo".into()),
            CellValue::String("Customer".into()),
            CellValue::String("Email".into()),
            CellValue::I64(4),
            CellValue::String("nvarchar".into()),
            CellValue::I64(400),
            CellValue::I64(0),
            CellValue::I64(0),
            CellValue::I64(1),
            CellValue::Null,
            CellValue::I64(0),
            CellValue::I64(0),
            CellValue::String("SQL_Latin1_General_CP1_CI_AS".into()),
        ]];
        let PageData::Properties(props) = parse_page_data(&target, EditorPage::General, &rows)
        else {
            panic!("expected properties");
        };
        assert_eq!(
            props,
            vec![
                property("Name", "Email"),
                property("Table", "Customer"),
                property("Position", "4"),
                property("Type", "nvarchar(400)"),
                property("Nullable", "Yes"),
                property("Identity", "No"),
                property("Computed", "No"),
                property("Collation", "SQL_Latin1_General_CP1_CI_AS"),
            ]
        );
    }

    /// A numeric column gets precision and scale; a `max` column says so; a
    /// default shows up as its own line.
    #[test]
    fn column_general_builds_the_type_line_and_shows_defaults() {
        let target = EditorTarget::new(EditorObjectKind::Column, "Sales", "dbo", "Total")
            .child_of("Orders".to_string());
        let numeric = vec![vec![
            CellValue::String("dbo".into()),
            CellValue::String("Orders".into()),
            CellValue::String("Total".into()),
            CellValue::I64(2),
            CellValue::String("decimal".into()),
            CellValue::I64(9),
            CellValue::I64(18),
            CellValue::I64(2),
            CellValue::I64(0),
            CellValue::String("((0))".into()),
            CellValue::I64(0),
            CellValue::I64(0),
            CellValue::Null,
        ]];
        let PageData::Properties(props) = parse_page_data(&target, EditorPage::General, &numeric)
        else {
            panic!("expected properties");
        };
        assert!(props.contains(&property("Type", "decimal(18,2)")));
        assert!(props.contains(&property("Nullable", "No")));
        assert!(props.contains(&property("Default", "((0))")));
        // No collation reported, so no collation line.
        assert!(!props.iter().any(|p| p == &property("Collation", "")));

        // `max_length = -1` is T-SQL's `(max)`.
        let max = vec![vec![
            CellValue::String("dbo".into()),
            CellValue::String("Orders".into()),
            CellValue::String("Notes".into()),
            CellValue::I64(3),
            CellValue::String("nvarchar".into()),
            CellValue::I64(-1),
            CellValue::I64(0),
            CellValue::I64(0),
            CellValue::I64(1),
            CellValue::Null,
            CellValue::I64(0),
            CellValue::I64(0),
            CellValue::Null,
        ]];
        let PageData::Properties(props) = parse_page_data(&target, EditorPage::General, &max)
        else {
            panic!("expected properties");
        };
        assert!(props.contains(&property("Type", "nvarchar(max)")));

        // MySQL already reports `varchar(200)`; it must not be decorated again.
        let mysql = vec![vec![
            CellValue::String("shop".into()),
            CellValue::String("orders".into()),
            CellValue::String("code".into()),
            CellValue::I64(1),
            CellValue::String("varchar(200)".into()),
            CellValue::I64(200),
            CellValue::Null,
            CellValue::Null,
            CellValue::I64(0),
            CellValue::Null,
            CellValue::I64(1),
            CellValue::I64(0),
            CellValue::Null,
        ]];
        let PageData::Properties(props) = parse_page_data(&target, EditorPage::General, &mysql)
        else {
            panic!("expected properties");
        };
        assert!(props.contains(&property("Type", "varchar(200)")));
        assert!(props.contains(&property("Identity", "Yes")));
    }

    #[test]
    fn schema_inventory_summarizes_and_preserves_rows() {
        let rows = vec![
            vec![
                CellValue::String("TABLE".into()),
                CellValue::String("reporting".into()),
                CellValue::String("DailySales".into()),
                CellValue::I64(42),
                CellValue::I64(8),
                CellValue::String("2026-01-01".into()),
                CellValue::String("2026-01-02".into()),
                CellValue::String("".into()),
            ],
            vec![
                CellValue::String("VIEW".into()),
                CellValue::String("reporting".into()),
                CellValue::String("SalesSummary".into()),
                CellValue::Null,
                CellValue::Null,
                CellValue::String("2026-01-01".into()),
                CellValue::String("2026-01-02".into()),
                CellValue::String("".into()),
            ],
        ];
        let PageData::Properties(summary) =
            parse_page_data(&schema_target(), EditorPage::General, &rows)
        else {
            panic!("expected properties");
        };
        assert!(
            summary
                .iter()
                .any(|row| row.label == "Objects" && row.value == "2")
        );
        assert!(
            summary
                .iter()
                .any(|row| row.label == "Includes" && row.value == "1 TABLE, 1 VIEW")
        );

        let PageData::Inventory(inventory) =
            parse_page_data(&schema_target(), EditorPage::Objects, &rows)
        else {
            panic!("expected inventory");
        };
        assert_eq!(inventory[0][2], "DailySales");
        assert_eq!(grid_columns(&PageData::Inventory(inventory)).len(), 8);
    }

    #[test]
    fn parses_columns_with_nullability_and_identity() {
        let rows = vec![vec![
            CellValue::String("Id".into()),
            CellValue::String("int".into()),
            CellValue::I64(4),
            CellValue::I64(10),
            CellValue::I64(0),
            CellValue::Bool(false),
            CellValue::Bool(true),
            CellValue::Bool(false),
        ]];
        let PageData::Columns(columns) =
            parse_page_data(&table_target(), EditorPage::Columns, &rows)
        else {
            panic!("expected columns");
        };
        assert_eq!(columns.len(), 1);
        assert!(columns[0].is_identity);
        assert_eq!(
            columns[0].grid_row(),
            vec!["Id", "int", "NOT NULL", "identity"]
        );
    }

    #[test]
    fn parses_index_columns_into_keys_and_includes() {
        let rows = vec![
            vec![
                CellValue::String("OrderDate".into()),
                CellValue::I64(1),
                CellValue::Bool(true),
                CellValue::Bool(false),
            ],
            vec![
                CellValue::String("Total".into()),
                CellValue::I64(0),
                CellValue::Bool(false),
                CellValue::Bool(true),
            ],
        ];
        let PageData::IndexColumns(columns) =
            parse_page_data(&index_target(), EditorPage::Columns, &rows)
        else {
            panic!("expected index columns");
        };
        assert_eq!(
            columns[0].grid_row(),
            vec!["OrderDate", "key", "1", "descending"]
        );
        assert_eq!(columns[1].grid_row(), vec!["Total", "include", "", ""]);
    }

    #[test]
    fn definition_page_reads_single_text_cell() {
        let rows = vec![vec![CellValue::String(
            "CREATE VIEW dbo.V AS SELECT 1".into(),
        )]];
        let target = EditorTarget::new(EditorObjectKind::View, "Sales", "dbo", "V");
        let PageData::Definition(def) = parse_page_data(&target, EditorPage::Definition, &rows)
        else {
            panic!("expected definition");
        };
        assert_eq!(def.as_deref(), Some("CREATE VIEW dbo.V AS SELECT 1"));
        assert!(parse_page_data(&target, EditorPage::Definition, &[]).is_empty());
    }

    #[test]
    fn staging_rename_against_original_clears_entry() {
        let mut changes = PendingChanges::default();
        changes.stage_rename("Orders", "Orders2").unwrap();
        assert_eq!(changes.pending_count(), 1);
        changes.stage_rename("Orders", "Orders").unwrap();
        assert!(!changes.is_dirty());
    }

    #[test]
    fn invalid_identifiers_are_rejected() {
        let mut changes = PendingChanges::default();
        assert!(changes.stage_rename("Orders", "   ").is_err());
        assert!(changes.stage_rename("Orders", &"x".repeat(129)).is_err());
        assert!(!changes.is_dirty());
    }

    #[test]
    fn nullability_toggle_captures_alter_fragment() {
        let column = ColumnInfo {
            name: "Notes".into(),
            type_name: "nvarchar".into(),
            max_length: Some(100),
            precision: None,
            scale: None,
            is_nullable: true,
            is_identity: false,
            is_computed: false,
        };
        let mut changes = PendingChanges::default();
        changes.stage_nullability(&column, false);
        assert_eq!(changes.pending_count(), 1);
        changes.stage_nullability(&column, true);
        assert!(!changes.is_dirty());
    }

    #[test]
    fn ddl_plan_orders_object_rename_last() {
        let mut changes = PendingChanges::default();
        changes.stage_rename("Orders", "OrdersArchive").unwrap();
        changes
            .stage_column_rename("Total", "Total", "OrderTotal")
            .unwrap();
        let plan = ddl_plan(&table_target(), &changes, false);
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0],
            "EXEC sp_rename N'dbo.Orders.Total', N'OrderTotal', N'COLUMN';"
        );
        assert_eq!(
            plan[1],
            "EXEC sp_rename N'dbo.Orders', N'OrdersArchive', N'OBJECT';"
        );
    }

    #[test]
    fn ddl_plan_for_postgres_uses_engine_forms() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let mut changes = PendingChanges::default();
        changes.stage_rename("Orders", "OrdersArchive").unwrap();
        changes
            .stage_column_rename("Total", "Total", "OrderTotal")
            .unwrap();
        let plan = ddl_plan_for(
            DatabaseType::Postgres,
            &table_target(),
            &changes,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            plan[0],
            "ALTER TABLE \"dbo\".\"Orders\" RENAME COLUMN \"Total\" TO \"OrderTotal\";"
        );
        assert_eq!(
            plan[1],
            "ALTER TABLE \"dbo\".\"Orders\" RENAME TO \"OrdersArchive\";"
        );
    }

    #[test]
    fn ddl_plan_for_postgres_nullability_uses_set_drop_not_null() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let column = ColumnInfo {
            name: "Notes".into(),
            type_name: "text".into(),
            max_length: None,
            precision: None,
            scale: None,
            is_nullable: true,
            is_identity: false,
            is_computed: false,
        };
        let mut changes = PendingChanges::default();
        changes.stage_nullability(&column, false);
        let plan = ddl_plan_for(
            DatabaseType::Postgres,
            &table_target(),
            &changes,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            plan,
            vec!["ALTER TABLE \"dbo\".\"Orders\" ALTER COLUMN \"Notes\" SET NOT NULL;"]
        );
    }

    #[test]
    fn ddl_plan_for_mysql_and_sqlite_shapes() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let mut changes = PendingChanges::default();
        changes.stage_rename("Orders", "OrdersArchive").unwrap();
        let my = ddl_plan_for(DatabaseType::MySql, &table_target(), &changes, false, None).unwrap();
        assert_eq!(my, vec!["RENAME TABLE `Orders` TO `OrdersArchive`;"]);
        let lite =
            ddl_plan_for(DatabaseType::Sqlite, &table_target(), &changes, false, None).unwrap();
        assert_eq!(
            lite,
            vec!["ALTER TABLE \"Orders\" RENAME TO \"OrdersArchive\";"]
        );
    }

    #[test]
    fn ddl_plan_for_rejects_inexpressible_changes_with_reasons() {
        use sqlly_lang::sqlmodel::DatabaseType;
        // SQLite cannot change nullability in place.
        let column = ColumnInfo {
            name: "Notes".into(),
            type_name: "TEXT".into(),
            max_length: None,
            precision: None,
            scale: None,
            is_nullable: true,
            is_identity: false,
            is_computed: false,
        };
        let mut changes = PendingChanges::default();
        changes.stage_nullability(&column, false);
        let err = ddl_plan_for(DatabaseType::Sqlite, &table_target(), &changes, false, None)
            .expect_err("sqlite nullability");
        assert!(err.contains("nullability"), "{err}");
        // Statistics options are SQL Server-only.
        let stats = PendingChanges {
            statistic_no_recompute: Some(true),
            ..Default::default()
        };
        let err = ddl_plan_for(DatabaseType::Postgres, &table_target(), &stats, false, None)
            .expect_err("pg statistics");
        assert!(err.contains("SQL Server-only"), "{err}");
        // Unsupported engine family gets one clear message.
        let err = ddl_plan_for(DatabaseType::Oracle, &table_target(), &changes, false, None)
            .expect_err("oracle unsupported");
        assert!(err.contains("not supported"), "{err}");
    }

    #[test]
    fn ddl_plan_for_maps_update_statistics_to_engine_analyze() {
        use sqlly_lang::sqlmodel::DatabaseType;
        let changes = PendingChanges {
            update_statistics_now: true,
            ..Default::default()
        };
        let pg = ddl_plan_for(
            DatabaseType::Postgres,
            &table_target(),
            &changes,
            false,
            None,
        )
        .unwrap();
        assert_eq!(pg, vec!["ANALYZE \"dbo\".\"Orders\";"]);
        let my = ddl_plan_for(DatabaseType::MySql, &table_target(), &changes, false, None).unwrap();
        assert_eq!(my, vec!["ANALYZE TABLE `Orders`;"]);
    }

    #[test]
    fn ddl_plan_emits_alter_column_with_full_type() {
        let column = ColumnInfo {
            name: "Notes".into(),
            type_name: "nvarchar".into(),
            max_length: Some(100),
            precision: None,
            scale: None,
            is_nullable: true,
            is_identity: false,
            is_computed: false,
        };
        let mut changes = PendingChanges::default();
        changes.stage_nullability(&column, false);
        let plan = ddl_plan(&table_target(), &changes, false);
        assert_eq!(
            plan,
            vec!["ALTER TABLE [dbo].[Orders] ALTER COLUMN [Notes] [nvarchar](50) NOT NULL;"]
        );
    }

    #[test]
    fn statistic_plan_uses_update_statistics_and_preserves_flag() {
        let target =
            EditorTarget::new(EditorObjectKind::Statistic, "Sales", "dbo", "Orders_Status")
                .child_of("Orders");
        let mut changes = PendingChanges::default();
        changes.stage_no_recompute(false, true);
        let plan = ddl_plan(&target, &changes, false);
        assert_eq!(
            plan,
            vec!["UPDATE STATISTICS [dbo] [Orders] [Orders_Status] WITH NORECOMPUTE;"]
        );

        let changes = PendingChanges {
            update_statistics_now: true,
            ..Default::default()
        };
        let plan = ddl_plan(&target, &changes, true);
        assert_eq!(
            plan,
            vec!["UPDATE STATISTICS [dbo] [Orders] [Orders_Status] WITH NORECOMPUTE;"],
            "update-now must not silently clear a live NORECOMPUTE flag"
        );
    }

    #[test]
    fn index_rename_uses_index_category_and_parent_path() {
        let mut changes = PendingChanges::default();
        changes
            .stage_rename("IX_Orders_Date", "IX_Orders_OrderDate")
            .unwrap();
        let plan = ddl_plan(&index_target(), &changes, false);
        assert_eq!(
            plan,
            vec!["EXEC sp_rename N'dbo.Orders.IX_Orders_Date', N'IX_Orders_OrderDate', N'INDEX';"]
        );
    }

    #[test]
    fn rename_escapes_single_quotes() {
        let mut changes = PendingChanges::default();
        changes.stage_rename("Orders", "O'Ryan").unwrap();
        let plan = ddl_plan(&table_target(), &changes, false);
        assert_eq!(
            plan,
            vec!["EXEC sp_rename N'dbo.Orders', N'O''Ryan', N'OBJECT';"]
        );
    }

    #[test]
    fn summary_lines_describe_every_change() {
        let mut changes = PendingChanges::default();
        changes.stage_rename("Orders", "Orders2").unwrap();
        changes.stage_no_recompute(false, true);
        changes.update_statistics_now = true;
        let lines = changes.summary_lines(EditorObjectKind::Statistic);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("Rename statistic to Orders2"));
        assert!(lines[1].contains("Disable automatic updates"));
        assert!(lines[2].contains("Update statistics now"));
    }

    fn s(text: &str) -> CellValue {
        CellValue::String(text.to_string())
    }

    fn inventory_rows() -> Vec<Vec<CellValue>> {
        // kind, schema, name, rows, size, created, modified, comment
        vec![
            vec![
                s("TABLE"),
                s("dbo"),
                s("Orders"),
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                s("Customer orders"),
            ],
            vec![
                s("VIEW"),
                s("dbo"),
                s("vOrders"),
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
            ],
            vec![
                s("TABLE"),
                s("sales"),
                s("Orders"),
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                s("Other schema's table"),
            ],
        ]
    }

    #[test]
    fn inventory_comment_matches_kind_schema_and_name() {
        let rows = inventory_rows();
        assert_eq!(
            object_comment_from_inventory(&rows, EditorObjectKind::Table, "dbo", "Orders")
                .as_deref(),
            Some("Customer orders")
        );
        // A NULL comment reads as unset.
        assert_eq!(
            object_comment_from_inventory(&rows, EditorObjectKind::View, "dbo", "vOrders"),
            None
        );
        // The same name in another schema does not leak across.
        assert_eq!(
            object_comment_from_inventory(&rows, EditorObjectKind::Table, "sales", "Orders")
                .as_deref(),
            Some("Other schema's table")
        );
        assert_eq!(
            object_comment_from_inventory(&rows, EditorObjectKind::Table, "dbo", "Missing"),
            None
        );
        // Only tables and views carry descriptions.
        assert_eq!(
            object_comment_from_inventory(&rows, EditorObjectKind::Procedure, "dbo", "Orders"),
            None
        );
    }

    #[test]
    fn an_empty_stored_description_updates_rather_than_adds_on_sql_server() {
        let rows = vec![vec![
            s("TABLE"),
            s("dbo"),
            s("Orders"),
            CellValue::Null,
            CellValue::Null,
            CellValue::Null,
            CellValue::Null,
            s(""),
        ]];
        let current =
            object_comment_from_inventory(&rows, EditorObjectKind::Table, "dbo", "Orders");
        assert_eq!(current.as_deref(), Some(""));
        let target = table_target();
        let mut staged = PendingChanges::default();
        staged.stage_description("", "Customer orders");
        let plan = ddl_plan_for(
            DatabaseType::SqlServer,
            &target,
            &staged,
            false,
            current.as_deref(),
        )
        .unwrap();
        assert!(
            plan[0].starts_with("EXEC sp_updateextendedproperty"),
            "{plan:?}"
        );
    }

    #[test]
    fn stage_description_unstages_when_back_to_original() {
        let mut changes = PendingChanges::default();
        changes.stage_description("old", "new");
        assert_eq!(changes.description.as_deref(), Some("new"));
        assert_eq!(changes.pending_count(), 1);
        changes.stage_description("old", "old");
        assert_eq!(changes.description, None);
        assert!(!changes.is_dirty());
    }

    #[test]
    fn description_plan_sql_server_picks_add_update_drop() {
        let target = table_target();
        let mut staged = PendingChanges::default();
        staged.stage_description("", "Customer orders");
        let add = ddl_plan_for(DatabaseType::SqlServer, &target, &staged, false, None).unwrap();
        assert_eq!(add.len(), 1);
        assert!(add[0].starts_with("EXEC sp_addextendedproperty"));
        assert!(add[0].contains("@level1type = N'TABLE'"));

        let update = ddl_plan_for(
            DatabaseType::SqlServer,
            &target,
            &staged,
            false,
            Some("previous text"),
        )
        .unwrap();
        assert!(update[0].starts_with("EXEC sp_updateextendedproperty"));

        let mut clearing = PendingChanges::default();
        clearing.stage_description("previous text", "");
        let drop = ddl_plan_for(
            DatabaseType::SqlServer,
            &target,
            &clearing,
            false,
            Some("previous text"),
        )
        .unwrap();
        assert!(drop[0].starts_with("EXEC sp_dropextendedproperty"));
    }

    #[test]
    fn description_plan_sql_server_addresses_views_as_views() {
        let target = EditorTarget::new(EditorObjectKind::View, "Sales", "dbo", "vOrders");
        let mut staged = PendingChanges::default();
        staged.stage_description("", "Order summary");
        let plan = ddl_plan_for(DatabaseType::SqlServer, &target, &staged, false, None).unwrap();
        assert!(plan[0].contains("@level1type = N'VIEW'"));
    }

    #[test]
    fn description_plan_postgres_comments_and_clears() {
        let target = table_target();
        let mut staged = PendingChanges::default();
        staged.stage_description("", "it's the orders table");
        let plan = ddl_plan_for(DatabaseType::Postgres, &target, &staged, false, None).unwrap();
        assert_eq!(
            plan[0],
            "COMMENT ON TABLE \"dbo\".\"Orders\" IS 'it''s the orders table';"
        );
        let mut clearing = PendingChanges::default();
        clearing.stage_description("x", "");
        let plan =
            ddl_plan_for(DatabaseType::Postgres, &target, &clearing, false, Some("x")).unwrap();
        assert_eq!(plan[0], "COMMENT ON TABLE \"dbo\".\"Orders\" IS NULL;");
        let view = EditorTarget::new(EditorObjectKind::View, "Sales", "dbo", "vOrders");
        let plan = ddl_plan_for(DatabaseType::Postgres, &view, &staged, false, None).unwrap();
        assert!(plan[0].starts_with("COMMENT ON VIEW "));
    }

    #[test]
    fn description_plan_mysql_table_only() {
        let target = table_target();
        let mut staged = PendingChanges::default();
        staged.stage_description("", "Customer orders");
        let plan = ddl_plan_for(DatabaseType::MySql, &target, &staged, false, None).unwrap();
        assert_eq!(plan[0], "ALTER TABLE `Orders` COMMENT = 'Customer orders';");
        let view = EditorTarget::new(EditorObjectKind::View, "Sales", "Sales", "vOrders");
        let err = ddl_plan_for(DatabaseType::MySql, &view, &staged, false, None).unwrap_err();
        assert!(err.contains("cannot store a description on a view"));
    }

    #[test]
    fn description_plan_sqlite_is_an_honest_error() {
        let target = table_target();
        let mut staged = PendingChanges::default();
        staged.stage_description("", "text");
        let err = ddl_plan_for(DatabaseType::Sqlite, &target, &staged, false, None).unwrap_err();
        assert!(err.contains("no server-side comments"));
    }

    #[test]
    fn description_only_plans_work_beyond_the_structural_engine_list() {
        let target = table_target();
        let mut staged = PendingChanges::default();
        staged.stage_description("", "Customer orders");
        let duck = ddl_plan_for(DatabaseType::DuckDb, &target, &staged, false, None).unwrap();
        assert!(duck[0].starts_with("COMMENT ON TABLE "));
        let oracle = ddl_plan_for(DatabaseType::Oracle, &target, &staged, false, None).unwrap();
        assert!(oracle[0].starts_with("COMMENT ON TABLE "));
        let ch = ddl_plan_for(DatabaseType::ClickHouse, &target, &staged, false, None).unwrap();
        assert!(ch[0].contains("MODIFY COMMENT 'Customer orders'"));
        // …while structural changes on those engines keep their existing refusal.
        staged.stage_rename("Orders", "Orders2").unwrap();
        assert!(ddl_plan_for(DatabaseType::Oracle, &target, &staged, false, None).is_err());
    }

    #[test]
    fn description_plan_rejects_non_table_view_kinds() {
        let target = EditorTarget::new(EditorObjectKind::Procedure, "Sales", "dbo", "pOrders");
        let mut staged = PendingChanges::default();
        staged.stage_description("", "text");
        let err = ddl_plan_for(DatabaseType::SqlServer, &target, &staged, false, None).unwrap_err();
        assert!(err.contains("tables and views"));
    }

    #[test]
    fn description_summary_lines_mention_set_and_clear() {
        let mut staged = PendingChanges::default();
        staged.stage_description("", "new text");
        let lines = staged.summary_lines(EditorObjectKind::Table);
        assert!(lines.iter().any(|l| l == "Set the table description"));
        let mut clearing = PendingChanges::default();
        clearing.stage_description("old", "");
        let lines = clearing.summary_lines(EditorObjectKind::Table);
        assert!(lines.iter().any(|l| l == "Clear the table description"));
    }

    #[test]
    fn statistic_no_recompute_reads_the_flag_not_translated_wording() {
        let row = |flag: bool| {
            vec![
                CellValue::String("st".into()),
                CellValue::Bool(false),
                CellValue::Bool(true),
                CellValue::Bool(flag),
                CellValue::Null,
            ]
        };
        let mut on = parse_statistic_general(&[row(true)]);
        assert!(statistic_no_recompute(&on));
        assert!(!statistic_no_recompute(&parse_statistic_general(&[row(
            false
        )])));
        // Relabelling (as a language switch would) does not change the answer.
        for property in &mut on {
            property.label = "otro".into();
            property.value = "sí".into();
        }
        assert!(statistic_no_recompute(&on));
        assert!(!statistic_no_recompute(&[]));
    }
}
