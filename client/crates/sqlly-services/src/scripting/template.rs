use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Where a template came from. Carried on every [`ScriptTemplate`] (rather
/// than inferred from a name set) so the UI can tell a user template from a
/// built-in even when both share a name — duplicates stay listed side by side
/// and each entry knows which one it is. `#[serde(default)]` keeps older
/// user-catalog files (written before the field existed) loadable; the
/// user-catalog loader re-stamps `User` on everything it reads regardless.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemplateOrigin {
    #[default]
    BuiltIn,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptTemplate {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub parameters: Vec<TemplateParameter>,
    #[serde(default)]
    pub origin: TemplateOrigin,
    /// Present when this template is also an explorer action: it appears in the
    /// schema tree's right-click menu for the object kinds it targets.
    /// `#[serde(default)]` keeps pre-action user catalogs loading with `None`.
    #[serde(default)]
    pub explorer_action: Option<ExplorerActionSpec>,
}

/// An object kind an explorer-action template can target. Mirrors the schema
/// tree's scriptable object kinds without depending on any UI type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExplorerObjectKind {
    Table,
    View,
    Procedure,
    Function,
}

impl ExplorerObjectKind {
    pub const ALL: [ExplorerObjectKind; 4] = [
        ExplorerObjectKind::Table,
        ExplorerObjectKind::View,
        ExplorerObjectKind::Procedure,
        ExplorerObjectKind::Function,
    ];

    /// The word the `{kind}` placeholder substitutes: singular, lowercase.
    pub fn label(self) -> &'static str {
        match self {
            ExplorerObjectKind::Table => "table",
            ExplorerObjectKind::View => "view",
            ExplorerObjectKind::Procedure => "procedure",
            ExplorerObjectKind::Function => "function",
        }
    }

    pub fn message(self) -> sqlly_core::DisplayMessage {
        let (key, fallback) = match self {
            Self::Table => ("services.scripting.explorer.kind_table", "table"),
            Self::View => ("services.scripting.explorer.kind_view", "view"),
            Self::Procedure => ("services.scripting.explorer.kind_procedure", "procedure"),
            Self::Function => ("services.scripting.explorer.kind_function", "function"),
        };
        sqlly_core::DisplayMessage::new(key, fallback)
    }

    pub fn plural_message(self) -> sqlly_core::DisplayMessage {
        let (key, fallback) = match self {
            Self::Table => ("services.scripting.explorer.kind_tables", "Tables"),
            Self::View => ("services.scripting.explorer.kind_views", "Views"),
            Self::Procedure => ("services.scripting.explorer.kind_procedures", "Procedures"),
            Self::Function => ("services.scripting.explorer.kind_functions", "Functions"),
        };
        sqlly_core::DisplayMessage::new(key, fallback)
    }

    /// Checkbox/menu caption for the kind.
    pub fn plural_label(self) -> &'static str {
        match self {
            ExplorerObjectKind::Table => "Tables",
            ExplorerObjectKind::View => "Views",
            ExplorerObjectKind::Procedure => "Procedures",
            ExplorerObjectKind::Function => "Functions",
        }
    }
}

/// What picking an explorer action does with the rendered SQL. `Script` (the
/// default) opens it in a new query tab for review; `Run` opens the tab and
/// executes it — straight away only when the SQL is read-only, otherwise
/// after the user confirms it in the review dialog (and every normal execute
/// gate still applies).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExplorerActionMode {
    #[default]
    Script,
    Run,
}

/// How a user template shows up in the explorer's right-click menu.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplorerActionSpec {
    /// Object kinds whose menus carry this action. An empty list (written by
    /// older builds) means every supported kind; the editor refuses to save
    /// an action with no kinds ticked.
    #[serde(default)]
    pub kinds: Vec<ExplorerObjectKind>,
    #[serde(default)]
    pub mode: ExplorerActionMode,
}

impl ExplorerActionSpec {
    pub fn matches(&self, kind: ExplorerObjectKind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind)
    }
}

/// The explorer object a template is rendered against: the values the
/// `{server}` / `{database}` / `{schema}` / `{object}` / `{kind}` placeholders
/// take. Fields the node cannot supply stay empty and substitute as "".
///
/// Values are *names*, never trusted SQL: [`ScriptTemplate::render_for_object`]
/// quotes them as identifiers in the target dialect by default, so a hostile
/// object name cannot inject statements.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TemplateObjectContext {
    /// Connection display name, as the explorer's server row shows it.
    pub server: String,
    pub database: String,
    pub schema: String,
    pub object: String,
    pub kind: Option<ExplorerObjectKind>,
}

/// How an object placeholder's value is written into the SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaceholderForm {
    /// `{object}`: a dialect-quoted identifier (`[Orders]`, `"Orders"`,
    /// `` `Orders` ``), embedded closers doubled.
    Identifier,
    /// `{object:literal}`: a dialect string literal (`N'Orders'`, `'Orders'`),
    /// embedded quotes (and MySQL backslashes) escaped.
    Literal,
    /// `{object:raw}`: the bare text with line breaks flattened to spaces so
    /// it can never end a `--` comment. No quoting or escaping otherwise.
    Raw,
}

/// Parses one object placeholder at the start of `rest` (which begins with
/// `{`): the token's byte length plus the value it substitutes, or `None`
/// when the braces hold anything else (literal text such as a JSON body).
fn object_placeholder(
    rest: &str,
    ctx: &TemplateObjectContext,
    db: sqlly_lang::sqlmodel::DatabaseType,
) -> Option<(usize, String)> {
    let close = rest.find('}')?;
    let inner = &rest[1..close];
    let (name, form) = match inner.split_once(':') {
        None => (inner, PlaceholderForm::Identifier),
        Some((name, "raw")) => (name, PlaceholderForm::Raw),
        Some((name, "literal")) => (name, PlaceholderForm::Literal),
        Some(_) => return None,
    };
    let value = match name {
        "server" => ctx.server.as_str(),
        "database" => ctx.database.as_str(),
        "schema" => ctx.schema.as_str(),
        "object" => ctx.object.as_str(),
        // A fixed lowercase word; only the bare form is recognised.
        "kind" if form == PlaceholderForm::Identifier => {
            return Some((close + 1, ctx.kind.map(|k| k.label()).unwrap_or("").into()));
        }
        _ => return None,
    };
    let rendered = match form {
        PlaceholderForm::Identifier if value.is_empty() => String::new(),
        PlaceholderForm::Identifier => db.quote_identifier(value),
        PlaceholderForm::Literal => db.string_literal(value),
        PlaceholderForm::Raw => flatten_line_breaks(value),
    };
    Some((close + 1, rendered))
}

/// Replaces every line break (CR, LF, and the Unicode line/paragraph
/// separators) with a space.
fn flatten_line_breaks(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\r' | '\n' | '\u{2028}' | '\u{2029}' | '\u{85}' => ' ',
            other => other,
        })
        .collect()
}

/// Single-pass template expansion: each `<name,type,default>` parameter and
/// (when `object` is supplied) each object placeholder is replaced exactly
/// once, left to right. Substituted text is never scanned again, so a value
/// that itself contains `{object}` or `<p,t,d>` stays literal.
fn expand_body(
    body: &str,
    parameters: &[TemplateParameter],
    values: &HashMap<String, String>,
    object: Option<(&TemplateObjectContext, sqlly_lang::sqlmodel::DatabaseType)>,
) -> String {
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        let rest = &body[i..];
        if rest.starts_with('{')
            && let Some((ctx, db)) = object
            && let Some((len, value)) = object_placeholder(rest, ctx, db)
        {
            out.push_str(&value);
            i += len;
            continue;
        }
        if rest.starts_with('<')
            && let Some(param) = parameters.iter().find(|p| {
                rest[1..].starts_with(p.name.as_str()) && rest[1 + p.name.len()..].starts_with(',')
            })
            && let Some(close) = rest.find('>')
        {
            out.push_str(values.get(&param.name).unwrap_or(&param.default_value));
            i += close + 1;
            continue;
        }
        let ch = rest.chars().next().expect("i is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateParameter {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default_value: String,
    #[serde(default)]
    pub data_type: String,
}

impl ScriptTemplate {
    pub fn name_message(&self) -> sqlly_core::DisplayMessage {
        let key = (self.origin == TemplateOrigin::BuiltIn)
            .then(|| built_in_copy_key(&self.name, "name"))
            .flatten();
        copy_message(key, &self.name)
    }

    pub fn category_message(&self) -> sqlly_core::DisplayMessage {
        let key = if self.category == USER_TEMPLATES_DEFAULT_CATEGORY {
            Some("services.scripting.templates.default_category")
        } else if self.origin == TemplateOrigin::BuiltIn {
            built_in_copy_key(&self.name, "category")
        } else {
            None
        };
        copy_message(key, &self.category)
    }

    pub fn description_message(&self) -> sqlly_core::DisplayMessage {
        let key = (self.origin == TemplateOrigin::BuiltIn)
            .then(|| built_in_copy_key(&self.name, "description"))
            .flatten();
        copy_message(key, &self.description)
    }

    pub fn parameter_description_message(&self, index: usize) -> sqlly_core::DisplayMessage {
        let Some(parameter) = self.parameters.get(index) else {
            return sqlly_core::DisplayMessage::raw("");
        };
        let key = (self.origin == TemplateOrigin::BuiltIn)
            .then(|| built_in_parameter_key(&self.name, &parameter.name))
            .flatten();
        copy_message(key, &parameter.description)
    }

    /// Renders the template body by substituting every SSMS-style placeholder
    /// `<name,type,default>` with the supplied value (falling back to the
    /// parameter's default). The same placeholder may appear multiple times in
    /// a body, so all occurrences are replaced — in one pass, so a supplied
    /// value is never itself re-substituted.
    pub fn render(&self, values: &HashMap<String, String>) -> String {
        expand_body(&self.body, &self.parameters, values, None)
    }

    /// The explorer-action spec when this template is one and targets `kind`.
    pub fn explorer_action_for(&self, kind: ExplorerObjectKind) -> Option<&ExplorerActionSpec> {
        self.explorer_action
            .as_ref()
            .filter(|spec| spec.matches(kind))
    }

    /// Renders the body against an explorer object, in `db`'s dialect.
    ///
    /// `{server}` / `{database}` / `{schema}` / `{object}` substitute as
    /// dialect-quoted identifiers (empty values stay empty); the `:literal`
    /// form (`{object:literal}`) substitutes a quoted, escaped string literal
    /// and the `:raw` form (`{object:raw}`) the bare text with line breaks
    /// flattened. `{kind}` is always the bare lowercase kind word. The
    /// SSMS-style `<name,type,default>` parameters render exactly as
    /// [`Self::render`] would (supplied values, defaults for the rest). Both
    /// syntaxes expand in a single left-to-right pass, so a hostile name that
    /// contains placeholder text is never re-expanded.
    pub fn render_for_object(
        &self,
        values: &HashMap<String, String>,
        ctx: &TemplateObjectContext,
        db: sqlly_lang::sqlmodel::DatabaseType,
    ) -> String {
        expand_body(&self.body, &self.parameters, values, Some((ctx, db)))
    }
}

/// The user templates flagged as explorer actions for `kind`, in catalog
/// order — the entries a matching object's right-click menu lists.
pub fn explorer_actions_for(
    templates: &[ScriptTemplate],
    kind: ExplorerObjectKind,
) -> Vec<&ScriptTemplate> {
    templates
        .iter()
        .filter(|t| t.explorer_action_for(kind).is_some())
        .collect()
}

fn copy_message(key: Option<&'static str>, fallback: &str) -> sqlly_core::DisplayMessage {
    match key {
        Some(key) => sqlly_core::DisplayMessage::new(key, fallback),
        None => sqlly_core::DisplayMessage::raw(fallback),
    }
}

fn built_in_copy_key(name: &str, field: &str) -> Option<&'static str> {
    let key = match (name, field) {
        ("Create Table", "name") => "services.scripting.templates.create_table.name",
        ("Create Table", "category") => "services.scripting.templates.create_table.category",
        ("Create Table", "description") => "services.scripting.templates.create_table.description",
        ("Create Stored Procedure", "name") => "services.scripting.templates.create_procedure.name",
        ("Create Stored Procedure", "category") => {
            "services.scripting.templates.create_procedure.category"
        }
        ("Create Stored Procedure", "description") => {
            "services.scripting.templates.create_procedure.description"
        }
        ("Create View", "name") => "services.scripting.templates.create_view.name",
        ("Create View", "category") => "services.scripting.templates.create_view.category",
        ("Create View", "description") => "services.scripting.templates.create_view.description",
        ("Create Nonclustered Index", "name") => "services.scripting.templates.create_index.name",
        ("Create Nonclustered Index", "category") => {
            "services.scripting.templates.create_index.category"
        }
        ("Create Nonclustered Index", "description") => {
            "services.scripting.templates.create_index.description"
        }
        ("Create Scalar Function", "name") => "services.scripting.templates.create_function.name",
        ("Create Scalar Function", "category") => {
            "services.scripting.templates.create_function.category"
        }
        ("Create Scalar Function", "description") => {
            "services.scripting.templates.create_function.description"
        }
        ("Add Column", "name") => "services.scripting.templates.add_column.name",
        ("Add Column", "category") => "services.scripting.templates.add_column.category",
        ("Add Column", "description") => "services.scripting.templates.add_column.description",
        ("Create Schema", "name") => "services.scripting.templates.create_schema.name",
        ("Create Schema", "category") => "services.scripting.templates.create_schema.category",
        ("Create Schema", "description") => {
            "services.scripting.templates.create_schema.description"
        }
        ("Create Trigger", "name") => "services.scripting.templates.create_trigger.name",
        ("Create Trigger", "category") => "services.scripting.templates.create_trigger.category",
        ("Create Trigger", "description") => {
            "services.scripting.templates.create_trigger.description"
        }
        _ => return None,
    };
    Some(key)
}

fn built_in_parameter_key(template: &str, parameter: &str) -> Option<&'static str> {
    let key = match (template, parameter) {
        ("Create Table", "schema_name") => {
            "services.scripting.templates.create_table.parameters.schema_name"
        }
        ("Create Table", "table_name") => {
            "services.scripting.templates.create_table.parameters.table_name"
        }
        ("Create Table", "column_name") => {
            "services.scripting.templates.create_table.parameters.column_name"
        }
        ("Create Stored Procedure", "schema_name") => {
            "services.scripting.templates.create_procedure.parameters.schema_name"
        }
        ("Create Stored Procedure", "procedure_name") => {
            "services.scripting.templates.create_procedure.parameters.procedure_name"
        }
        ("Create Stored Procedure", "param_name") => {
            "services.scripting.templates.create_procedure.parameters.param_name"
        }
        ("Create Stored Procedure", "param_type") => {
            "services.scripting.templates.create_procedure.parameters.param_type"
        }
        ("Create Stored Procedure", "param_default") => {
            "services.scripting.templates.create_procedure.parameters.param_default"
        }
        ("Create View", "schema_name") => {
            "services.scripting.templates.create_view.parameters.schema_name"
        }
        ("Create View", "view_name") => {
            "services.scripting.templates.create_view.parameters.view_name"
        }
        ("Create View", "column_list") => {
            "services.scripting.templates.create_view.parameters.column_list"
        }
        ("Create View", "source_table") => {
            "services.scripting.templates.create_view.parameters.source_table"
        }
        ("Create Nonclustered Index", "schema_name") => {
            "services.scripting.templates.create_index.parameters.schema_name"
        }
        ("Create Nonclustered Index", "table_name") => {
            "services.scripting.templates.create_index.parameters.table_name"
        }
        ("Create Nonclustered Index", "column_name") => {
            "services.scripting.templates.create_index.parameters.column_name"
        }
        ("Create Scalar Function", "schema_name") => {
            "services.scripting.templates.create_function.parameters.schema_name"
        }
        ("Create Scalar Function", "function_name") => {
            "services.scripting.templates.create_function.parameters.function_name"
        }
        ("Create Scalar Function", "param_name") => {
            "services.scripting.templates.create_function.parameters.param_name"
        }
        ("Create Scalar Function", "param_type") => {
            "services.scripting.templates.create_function.parameters.param_type"
        }
        ("Create Scalar Function", "return_type") => {
            "services.scripting.templates.create_function.parameters.return_type"
        }
        ("Add Column", "schema_name") => {
            "services.scripting.templates.add_column.parameters.schema_name"
        }
        ("Add Column", "table_name") => {
            "services.scripting.templates.add_column.parameters.table_name"
        }
        ("Add Column", "column_name") => {
            "services.scripting.templates.add_column.parameters.column_name"
        }
        ("Add Column", "data_type") => {
            "services.scripting.templates.add_column.parameters.data_type"
        }
        ("Add Column", "nullability") => {
            "services.scripting.templates.add_column.parameters.nullability"
        }
        ("Create Schema", "schema_name") => {
            "services.scripting.templates.create_schema.parameters.schema_name"
        }
        ("Create Schema", "owner_name") => {
            "services.scripting.templates.create_schema.parameters.owner_name"
        }
        ("Create Trigger", "schema_name") => {
            "services.scripting.templates.create_trigger.parameters.schema_name"
        }
        ("Create Trigger", "trigger_name") => {
            "services.scripting.templates.create_trigger.parameters.trigger_name"
        }
        ("Create Trigger", "table_name") => {
            "services.scripting.templates.create_trigger.parameters.table_name"
        }
        ("Create Trigger", "trigger_event") => {
            "services.scripting.templates.create_trigger.parameters.trigger_event"
        }
        _ => return None,
    };
    Some(key)
}

pub fn built_in_templates() -> Vec<ScriptTemplate> {
    vec![
        ScriptTemplate {
            origin: TemplateOrigin::BuiltIn,
            explorer_action: None,
            name: "Create Table".to_string(),
            category: "Table".to_string(),
            description: "Create a new table".to_string(),
            body: "CREATE TABLE <schema_name,,dbo>.<table_name,,NewTable>\n(\n    <column_name,,id> INT NOT NULL IDENTITY(1,1),\n    CONSTRAINT PK_<table_name,,NewTable> PRIMARY KEY (<column_name,,id>)\n)".to_string(),
            parameters: vec![
                TemplateParameter {
                    name: "schema_name".to_string(),
                    description: "Schema name".to_string(),
                    default_value: "dbo".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "table_name".to_string(),
                    description: "Table name".to_string(),
                    default_value: "NewTable".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "column_name".to_string(),
                    description: "Primary key column name".to_string(),
                    default_value: "id".to_string(),
                    data_type: "sysname".to_string(),
                },
            ],
        },
        ScriptTemplate {
            origin: TemplateOrigin::BuiltIn,
            explorer_action: None,
            name: "Create Stored Procedure".to_string(),
            category: "Stored Procedure".to_string(),
            description: "Create a new stored procedure".to_string(),
            body: "CREATE PROCEDURE <schema_name,,dbo>.<procedure_name,,NewProcedure>\n    @<param_name,,@Param1> <param_type,,INT> = <param_default,,NULL>\nAS\nBEGIN\n    SET NOCOUNT ON;\n    -- Procedure body\n    SELECT 1\nEND".to_string(),
            parameters: vec![
                TemplateParameter {
                    name: "schema_name".to_string(),
                    description: "Schema name".to_string(),
                    default_value: "dbo".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "procedure_name".to_string(),
                    description: "Procedure name".to_string(),
                    default_value: "NewProcedure".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "param_name".to_string(),
                    description: "Parameter name".to_string(),
                    default_value: "@Param1".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "param_type".to_string(),
                    description: "Parameter type".to_string(),
                    default_value: "INT".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "param_default".to_string(),
                    description: "Parameter default value".to_string(),
                    default_value: "NULL".to_string(),
                    data_type: "nvarchar(128)".to_string(),
                },
            ],
        },
        ScriptTemplate {
            origin: TemplateOrigin::BuiltIn,
            explorer_action: None,
            name: "Create View".to_string(),
            category: "View".to_string(),
            description: "Create a new view".to_string(),
            body: "CREATE VIEW <schema_name,,dbo>.<view_name,,NewView>\nAS\nSELECT <column_list,,*>\nFROM <schema_name,,dbo>.<source_table,,SourceTable>".to_string(),
            parameters: vec![
                TemplateParameter {
                    name: "schema_name".to_string(),
                    description: "Schema name".to_string(),
                    default_value: "dbo".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "view_name".to_string(),
                    description: "View name".to_string(),
                    default_value: "NewView".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "column_list".to_string(),
                    description: "Columns to select".to_string(),
                    default_value: "*".to_string(),
                    data_type: "nvarchar(max)".to_string(),
                },
                TemplateParameter {
                    name: "source_table".to_string(),
                    description: "Source table".to_string(),
                    default_value: "SourceTable".to_string(),
                    data_type: "sysname".to_string(),
                },
            ],
        },
        ScriptTemplate {
            origin: TemplateOrigin::BuiltIn,
            explorer_action: None,
            name: "Create Nonclustered Index".to_string(),
            category: "Index".to_string(),
            description: "Create a nonclustered index on a table".to_string(),
            body: "CREATE NONCLUSTERED INDEX IX_<table_name,,MyTable>_<column_name,,MyColumn>\nON <schema_name,,dbo>.<table_name,,MyTable> (<column_name,,MyColumn>)".to_string(),
            parameters: vec![
                TemplateParameter {
                    name: "schema_name".to_string(),
                    description: "Schema name".to_string(),
                    default_value: "dbo".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "table_name".to_string(),
                    description: "Table name".to_string(),
                    default_value: "MyTable".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "column_name".to_string(),
                    description: "Indexed column".to_string(),
                    default_value: "MyColumn".to_string(),
                    data_type: "sysname".to_string(),
                },
            ],
        },
        ScriptTemplate {
            origin: TemplateOrigin::BuiltIn,
            explorer_action: None,
            name: "Create Scalar Function".to_string(),
            category: "Function".to_string(),
            description: "Create a scalar user-defined function".to_string(),
            body: "CREATE FUNCTION <schema_name,,dbo>.<function_name,,NewFunction>\n(\n    @<param_name,,Input> <param_type,,INT>\n)\nRETURNS <return_type,,INT>\nAS\nBEGIN\n    RETURN @<param_name,,Input>\nEND".to_string(),
            parameters: vec![
                TemplateParameter {
                    name: "schema_name".to_string(),
                    description: "Schema name".to_string(),
                    default_value: "dbo".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "function_name".to_string(),
                    description: "Function name".to_string(),
                    default_value: "NewFunction".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "param_name".to_string(),
                    description: "Parameter name (without @)".to_string(),
                    default_value: "Input".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "param_type".to_string(),
                    description: "Parameter type".to_string(),
                    default_value: "INT".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "return_type".to_string(),
                    description: "Return type".to_string(),
                    default_value: "INT".to_string(),
                    data_type: "sysname".to_string(),
                },
            ],
        },
        ScriptTemplate {
            origin: TemplateOrigin::BuiltIn,
            explorer_action: None,
            name: "Add Column".to_string(),
            category: "Table".to_string(),
            description: "Add a column to an existing table".to_string(),
            body: "ALTER TABLE <schema_name,,dbo>.<table_name,,MyTable>\nADD <column_name,,NewColumn> <data_type,,NVARCHAR(100)> <nullability,,NULL>".to_string(),
            parameters: vec![
                TemplateParameter {
                    name: "schema_name".to_string(),
                    description: "Schema name".to_string(),
                    default_value: "dbo".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "table_name".to_string(),
                    description: "Table name".to_string(),
                    default_value: "MyTable".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "column_name".to_string(),
                    description: "New column name".to_string(),
                    default_value: "NewColumn".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "data_type".to_string(),
                    description: "Column data type".to_string(),
                    default_value: "NVARCHAR(100)".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "nullability".to_string(),
                    description: "NULL or NOT NULL".to_string(),
                    default_value: "NULL".to_string(),
                    data_type: "sysname".to_string(),
                },
            ],
        },
        ScriptTemplate {
            origin: TemplateOrigin::BuiltIn,
            explorer_action: None,
            name: "Create Schema".to_string(),
            category: "Schema".to_string(),
            description: "Create a new schema with an owner".to_string(),
            body: "CREATE SCHEMA <schema_name,,NewSchema> AUTHORIZATION <owner_name,,dbo>".to_string(),
            parameters: vec![
                TemplateParameter {
                    name: "schema_name".to_string(),
                    description: "Schema name".to_string(),
                    default_value: "NewSchema".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "owner_name".to_string(),
                    description: "Owning principal".to_string(),
                    default_value: "dbo".to_string(),
                    data_type: "sysname".to_string(),
                },
            ],
        },
        ScriptTemplate {
            origin: TemplateOrigin::BuiltIn,
            explorer_action: None,
            name: "Create Trigger".to_string(),
            category: "Trigger".to_string(),
            description: "Create an AFTER trigger on a table".to_string(),
            body: "CREATE TRIGGER <schema_name,,dbo>.<trigger_name,,NewTrigger>\nON <schema_name,,dbo>.<table_name,,MyTable>\nAFTER <trigger_event,,INSERT>\nAS\nBEGIN\n    SET NOCOUNT ON;\n    -- Trigger body\nEND".to_string(),
            parameters: vec![
                TemplateParameter {
                    name: "schema_name".to_string(),
                    description: "Schema name".to_string(),
                    default_value: "dbo".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "trigger_name".to_string(),
                    description: "Trigger name".to_string(),
                    default_value: "NewTrigger".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "table_name".to_string(),
                    description: "Table the trigger watches".to_string(),
                    default_value: "MyTable".to_string(),
                    data_type: "sysname".to_string(),
                },
                TemplateParameter {
                    name: "trigger_event".to_string(),
                    description: "INSERT, UPDATE, or DELETE".to_string(),
                    default_value: "INSERT".to_string(),
                    data_type: "sysname".to_string(),
                },
            ],
        },
    ]
}

/// Current schema version written by [`UserTemplatesFile::to_json`].
pub const USER_TEMPLATES_VERSION: u32 = 1;

/// Default category for user templates saved without one.
pub const USER_TEMPLATES_DEFAULT_CATEGORY: &str = "My Templates";

/// The on-disk user template catalog: a versioned JSON document so future
/// schema changes can migrate old files instead of dropping them. Pure
/// to/from-JSON helpers — where the file lives is the host layer's business.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserTemplatesFile {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub templates: Vec<ScriptTemplate>,
}

impl UserTemplatesFile {
    /// Wraps a template list at the current schema version.
    pub fn new(templates: Vec<ScriptTemplate>) -> Self {
        Self {
            version: USER_TEMPLATES_VERSION,
            templates,
        }
    }

    /// Tolerant parse: unreadable or partial JSON yields an empty catalog
    /// rather than an error (a corrupt user file must not break the explorer).
    /// Every template read from a user catalog is stamped
    /// [`TemplateOrigin::User`], whatever the file claims.
    pub fn from_json(text: &str) -> Self {
        Self::try_from_json(text).unwrap_or_default()
    }

    /// Strict parse for persistence paths: corrupt JSON is an error so saves
    /// cannot overwrite a damaged catalog with an empty default.
    pub fn try_from_json(text: &str) -> Result<Self, String> {
        let mut file = serde_json::from_str::<Self>(text).map_err(|e| e.to_string())?;
        for template in &mut file.templates {
            template.origin = TemplateOrigin::User;
        }
        Ok(file)
    }

    /// Pretty-printed JSON for persisting; the version field is always the
    /// current [`USER_TEMPLATES_VERSION`].
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&Self {
            version: USER_TEMPLATES_VERSION,
            templates: self.templates.clone(),
        })
        .unwrap_or_else(|_| "{}".to_string())
    }
}

/// Derives a template's parameter list from SSMS-style `<name,type,default>`
/// placeholders in its body, in order of first appearance, deduplicated by
/// name. Used when saving a user-authored template so the author only writes
/// the body. Guards against SQL's own `<`/`>` (comparisons, generics in
/// comments): a placeholder must be on one line, contain at least two commas,
/// and have a non-empty name with no nested angle brackets.
pub fn parameters_from_body(body: &str) -> Vec<TemplateParameter> {
    let mut params: Vec<TemplateParameter> = Vec::new();
    let mut search_from = 0;
    while let Some(rel_start) = body[search_from..].find('<') {
        let start = search_from + rel_start;
        let Some(rel_end) = body[start + 1..].find('>') else {
            break;
        };
        let end = start + 1 + rel_end;
        let content = &body[start + 1..end];
        search_from = start + 1;
        if content.contains('<') || content.contains('\n') {
            continue;
        }
        let mut parts = content.splitn(3, ',');
        let (Some(name), Some(data_type), Some(default_value)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        if params.iter().any(|p| p.name == name) {
            continue;
        }
        params.push(TemplateParameter {
            name: name.to_string(),
            description: String::new(),
            default_value: default_value.to_string(),
            data_type: data_type.to_string(),
        });
        search_from = end + 1;
    }
    params
}

/// A category node in the Template Explorer tree, owning the templates filed
/// under it. Mirrors SSMS's Template Browser, where templates are organised
/// under expandable folders such as "Table" or "Stored Procedure".
#[derive(Debug, Clone)]
pub struct TemplateCategory {
    pub name: String,
    pub templates: Vec<ScriptTemplate>,
}

/// A browsable catalog of script templates, grouped by category and searchable
/// by name/description. The UI binds to this to render the explorer tree and
/// its filter box; the core owns the grouping and search semantics.
#[derive(Debug, Clone)]
pub struct TemplateExplorer {
    templates: Vec<ScriptTemplate>,
}

impl TemplateExplorer {
    /// Builds an explorer over an arbitrary template set.
    pub fn new(templates: Vec<ScriptTemplate>) -> Self {
        Self { templates }
    }

    /// Builds the explorer over the application's built-in templates.
    pub fn with_built_in() -> Self {
        Self::new(built_in_templates())
    }

    /// Builds the explorer over the built-in catalog merged with the user's
    /// own templates. User templates come first: they win name collisions in
    /// [`Self::find`] (both entries stay listed), and [`Self::categories`]
    /// sorts categories containing user templates ahead of built-in-only
    /// ones. Each user template is stamped [`TemplateOrigin::User`]; an empty
    /// category is filed under [`USER_TEMPLATES_DEFAULT_CATEGORY`].
    pub fn with_built_in_and(user: Vec<ScriptTemplate>) -> Self {
        let mut templates: Vec<ScriptTemplate> = user
            .into_iter()
            .map(|mut t| {
                t.origin = TemplateOrigin::User;
                if t.category.trim().is_empty() {
                    t.category = USER_TEMPLATES_DEFAULT_CATEGORY.to_string();
                }
                t
            })
            .collect();
        templates.extend(built_in_templates());
        Self::new(templates)
    }

    pub fn templates(&self) -> &[ScriptTemplate] {
        &self.templates
    }

    /// Distinct category names for the top level of the explorer tree:
    /// categories containing at least one user template first, then the rest,
    /// each group in case-insensitive alphabetical order. With no user
    /// templates this is plain alphabetical order, as before.
    pub fn categories(&self) -> Vec<TemplateCategory> {
        let mut names: Vec<String> = Vec::new();
        for template in &self.templates {
            if !names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&template.category))
            {
                names.push(template.category.clone());
            }
        }
        let has_user = |name: &str| {
            self.templates
                .iter()
                .any(|t| t.origin == TemplateOrigin::User && t.category.eq_ignore_ascii_case(name))
        };
        names.sort_by_key(|a| (!has_user(a), a.to_ascii_lowercase()));
        names
            .into_iter()
            .map(|name| TemplateCategory {
                templates: self
                    .templates
                    .iter()
                    .filter(|t| t.category.eq_ignore_ascii_case(&name))
                    .cloned()
                    .collect(),
                name,
            })
            .collect()
    }

    /// Looks up a single template by its exact name.
    pub fn find(&self, name: &str) -> Option<&ScriptTemplate> {
        self.templates.iter().find(|t| t.name == name)
    }

    /// Case-insensitive substring search across template name, category, and
    /// description, powering the explorer's filter box.
    pub fn search(&self, query: &str) -> Vec<&ScriptTemplate> {
        let needle = query.to_ascii_lowercase();
        if needle.is_empty() {
            return self.templates.iter().collect();
        }
        self.templates
            .iter()
            .filter(|t| {
                t.name.to_ascii_lowercase().contains(&needle)
                    || t.category.to_ascii_lowercase().contains(&needle)
                    || t.description.to_ascii_lowercase().contains(&needle)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_lang::sqlmodel::DatabaseType;

    #[test]
    fn renders_ssms_placeholders_with_supplied_and_default_values() {
        let template = built_in_templates()
            .into_iter()
            .find(|t| t.name == "Create Table")
            .expect("create table template");

        let mut values = HashMap::new();
        values.insert("schema_name".to_string(), "sales".to_string());
        values.insert("table_name".to_string(), "Orders".to_string());
        // column_name intentionally omitted to exercise the default ("id").

        let rendered = template.render(&values);

        assert!(rendered.contains("CREATE TABLE sales.Orders"));
        // table_name appears twice (table + PK constraint name); both replaced.
        assert!(rendered.contains("CONSTRAINT PK_Orders PRIMARY KEY (id)"));
        assert!(
            !rendered.contains('<'),
            "no placeholder should remain: {rendered}"
        );
    }

    #[test]
    fn render_substitutes_repeated_placeholders_once_each() {
        let body = "<table_name,sysname,NewTable> ... PK_<table_name,sysname,NewTable>";
        let template = ScriptTemplate {
            name: "t".into(),
            category: String::new(),
            description: String::new(),
            body: body.into(),
            parameters: parameters_from_body(body),
            origin: TemplateOrigin::User,
            explorer_action: None,
        };
        assert_eq!(template.render(&HashMap::new()), "NewTable ... PK_NewTable");
    }

    #[test]
    fn render_never_resubstitutes_a_supplied_value() {
        // Chained `.replace` calls used to expand `<b,…>` inside a's value.
        let body = "<a,int,1> <b,int,2>";
        let template = ScriptTemplate {
            name: "t".into(),
            category: String::new(),
            description: String::new(),
            body: body.into(),
            parameters: parameters_from_body(body),
            origin: TemplateOrigin::User,
            explorer_action: None,
        };
        let values = HashMap::from([("a".to_string(), "<b,int,2>".to_string())]);
        assert_eq!(template.render(&values), "<b,int,2> 2");
    }

    #[test]
    fn built_in_templates_have_parameters() {
        let templates = built_in_templates();
        assert!(templates.len() >= 2);
        assert!(templates.iter().all(|t| !t.parameters.is_empty()));
        for (index, template) in templates.iter().enumerate() {
            assert!(template.name_message().key().is_some(), "name {index}");
            assert!(
                template.category_message().key().is_some(),
                "category {index}"
            );
            assert!(
                template.description_message().key().is_some(),
                "description {index}"
            );
            for parameter in 0..template.parameters.len() {
                assert!(
                    template
                        .parameter_description_message(parameter)
                        .key()
                        .is_some(),
                    "parameter {parameter} of template {index}"
                );
            }
        }
        let user = user_template("Custom", "Mine");
        assert_eq!(user.name_message().key(), None);
        assert_eq!(user.category_message().key(), None);
    }

    #[test]
    fn explorer_groups_built_in_templates_into_sorted_categories() {
        let explorer = TemplateExplorer::with_built_in();
        let categories = explorer.categories();
        assert_eq!(
            categories
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Function",
                "Index",
                "Schema",
                "Stored Procedure",
                "Table",
                "Trigger",
                "View"
            ]
        );
        assert!(categories.iter().all(|c| !c.templates.is_empty()));
    }

    #[test]
    fn explorer_search_matches_name_category_and_description() {
        let explorer = TemplateExplorer::with_built_in();
        assert!(
            explorer
                .search("procedure")
                .iter()
                .any(|t| t.name == "Create Stored Procedure")
        );
        // "TABLE" matches the Table-category templates plus any template whose
        // description mentions tables (index/trigger reference their table).
        assert!(explorer.search("TABLE").len() >= 2);
        // Empty query returns the full catalog.
        assert_eq!(explorer.search("").len(), explorer.templates().len());
        assert_eq!(explorer.search("nonexistent").len(), 0);
    }

    #[test]
    fn explorer_find_returns_template_by_name() {
        let explorer = TemplateExplorer::with_built_in();
        assert!(explorer.find("Create Table").is_some());
        assert!(explorer.find("Nope").is_none());
    }

    fn user_template(name: &str, category: &str) -> ScriptTemplate {
        ScriptTemplate {
            name: name.to_string(),
            category: category.to_string(),
            description: "mine".to_string(),
            body: "SELECT <top_n,int,10>".to_string(),
            parameters: parameters_from_body("SELECT <top_n,int,10>"),
            origin: TemplateOrigin::User,
            explorer_action: None,
        }
    }

    #[test]
    fn user_templates_file_round_trips_json() {
        let file = UserTemplatesFile::new(vec![user_template("Top N", "Snippets")]);
        let json = file.to_json();
        let restored = UserTemplatesFile::from_json(&json);
        assert_eq!(restored.version, USER_TEMPLATES_VERSION);
        assert_eq!(restored, file);
    }

    #[test]
    fn user_templates_file_tolerates_old_and_bad_json() {
        // Garbage → empty catalog for display, never an error.
        assert!(
            UserTemplatesFile::from_json("not json")
                .templates
                .is_empty()
        );
        assert!(UserTemplatesFile::try_from_json("not json").is_err());
        // A pre-`origin`, sparse template still loads (serde defaults) and is
        // stamped as a user template.
        let old = r#"{"version":1,"templates":[{"name":"Mine","body":"SELECT 1"}]}"#;
        let file = UserTemplatesFile::from_json(old);
        assert_eq!(file.templates.len(), 1);
        assert_eq!(file.templates[0].name, "Mine");
        assert_eq!(file.templates[0].origin, TemplateOrigin::User);
        assert!(file.templates[0].category.is_empty());
        assert_eq!(UserTemplatesFile::try_from_json(old).unwrap(), file);
    }

    #[test]
    fn merged_explorer_lists_user_templates_and_their_categories_first() {
        let explorer = TemplateExplorer::with_built_in_and(vec![
            user_template("Top N", "Snippets"),
            user_template("Uncategorised", ""),
        ]);
        // User templates precede built-ins in the flat list.
        assert_eq!(explorer.templates()[0].name, "Top N");
        assert_eq!(explorer.templates()[1].name, "Uncategorised");
        assert_eq!(explorer.templates()[1].category, "My Templates");
        assert_eq!(explorer.templates()[1].origin, TemplateOrigin::User);
        // User categories sort ahead of the built-in ones.
        let names: Vec<String> = explorer.categories().into_iter().map(|c| c.name).collect();
        assert_eq!(names[0], "My Templates");
        assert_eq!(names[1], "Snippets");
        assert!(names[2..].iter().all(|n| n != "Snippets"));
    }

    #[test]
    fn duplicate_name_user_template_wins_in_find_but_both_stay_listed() {
        let mine = user_template("Create Table", "Snippets");
        let explorer = TemplateExplorer::with_built_in_and(vec![mine]);
        let found = explorer.find("Create Table").expect("find");
        assert_eq!(found.origin, TemplateOrigin::User);
        assert_eq!(found.category, "Snippets");
        assert_eq!(
            explorer
                .templates()
                .iter()
                .filter(|t| t.name == "Create Table")
                .count(),
            2
        );
    }

    #[test]
    fn parameters_from_body_parses_in_order_and_dedupes() {
        let body = "SELECT TOP <top_n,int,10> * FROM <schema,sysname,dbo>.<table,sysname,T>\nWHERE id > <top_n,int,10>";
        let params = parameters_from_body(body);
        assert_eq!(
            params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["top_n", "schema", "table"]
        );
        assert_eq!(params[0].data_type, "int");
        assert_eq!(params[0].default_value, "10");
        // Defaults may themselves contain commas: only the first two split.
        let params = parameters_from_body("<cols,nvarchar(max),a, b, c>");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].default_value, "a, b, c");
    }

    #[test]
    fn parameters_from_body_ignores_comparisons_and_malformed_placeholders() {
        assert!(parameters_from_body("WHERE a < b AND c > d").is_empty());
        assert!(parameters_from_body("WHERE a < b").is_empty());
        // Fewer than two commas is not a placeholder.
        assert!(parameters_from_body("<no_default,int>").is_empty());
        // Placeholders never span lines, and empty names are rejected.
        assert!(parameters_from_body("<a,\nb,c>").is_empty());
        assert!(parameters_from_body("<,int,1>").is_empty());
        // A stray '<' before a real placeholder does not swallow it.
        let params = parameters_from_body("WHERE a < <top_n,int,10>");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "top_n");
    }

    #[test]
    fn parsed_parameters_render_back_through_the_template() {
        let body = "SELECT TOP <top_n,int,10> * FROM t";
        let template = ScriptTemplate {
            name: "Top".to_string(),
            category: String::new(),
            description: String::new(),
            body: body.to_string(),
            parameters: parameters_from_body(body),
            origin: TemplateOrigin::User,
            explorer_action: None,
        };
        let mut values = HashMap::new();
        values.insert("top_n".to_string(), "25".to_string());
        assert_eq!(template.render(&values), "SELECT TOP 25 * FROM t");
        assert_eq!(template.render(&HashMap::new()), "SELECT TOP 10 * FROM t");
    }

    fn action_template(kinds: Vec<ExplorerObjectKind>, mode: ExplorerActionMode) -> ScriptTemplate {
        ScriptTemplate {
            name: "Row count".to_string(),
            category: "Snippets".to_string(),
            description: String::new(),
            body: "-- {kind} {object} on {server}/{database}\nSELECT COUNT(*) FROM {schema}.{object} WHERE 1 = <one,int,1>".to_string(),
            parameters: parameters_from_body(
                "-- {kind} {object} on {server}/{database}\nSELECT COUNT(*) FROM {schema}.{object} WHERE 1 = <one,int,1>",
            ),
            origin: TemplateOrigin::User,
            explorer_action: Some(ExplorerActionSpec { kinds, mode }),
        }
    }

    fn orders_ctx() -> TemplateObjectContext {
        TemplateObjectContext {
            server: "prod-sql".to_string(),
            database: "shop".to_string(),
            schema: "dbo".to_string(),
            object: "Orders".to_string(),
            kind: Some(ExplorerObjectKind::Table),
        }
    }

    #[test]
    fn render_for_object_quotes_identifiers_and_renders_ssms_params() {
        let template = action_template(vec![ExplorerObjectKind::Table], ExplorerActionMode::Script);
        let rendered =
            template.render_for_object(&HashMap::new(), &orders_ctx(), DatabaseType::SqlServer);
        assert_eq!(
            rendered,
            "-- table [Orders] on [prod-sql]/[shop]\nSELECT COUNT(*) FROM [dbo].[Orders] WHERE 1 = 1"
        );
        let pg = template.render_for_object(&HashMap::new(), &orders_ctx(), DatabaseType::Postgres);
        assert!(pg.ends_with("FROM \"dbo\".\"Orders\" WHERE 1 = 1"), "{pg}");
        let my = template.render_for_object(&HashMap::new(), &orders_ctx(), DatabaseType::MySql);
        assert!(my.ends_with("FROM `dbo`.`Orders` WHERE 1 = 1"), "{my}");
    }

    #[test]
    fn render_for_object_leaves_plain_render_untouched() {
        // Without an object context, brace placeholders survive `render`
        // verbatim — only the SSMS parameters substitute.
        let template = action_template(vec![], ExplorerActionMode::Script);
        let rendered = template.render(&HashMap::new());
        assert!(rendered.contains("{object}"));
        assert!(rendered.contains("WHERE 1 = 1"));
    }

    #[test]
    fn render_for_object_substitutes_empty_for_unsupplied_context() {
        let template = action_template(vec![], ExplorerActionMode::Script);
        let rendered = template.render_for_object(
            &HashMap::new(),
            &TemplateObjectContext::default(),
            DatabaseType::SqlServer,
        );
        assert_eq!(rendered, "--   on /\nSELECT COUNT(*) FROM . WHERE 1 = 1");
    }

    fn body_template(body: &str) -> ScriptTemplate {
        ScriptTemplate {
            name: "t".into(),
            category: String::new(),
            description: String::new(),
            body: body.into(),
            parameters: parameters_from_body(body),
            origin: TemplateOrigin::User,
            explorer_action: None,
        }
    }

    #[test]
    fn hostile_object_names_cannot_inject_statements() {
        let template = body_template("SELECT COUNT(*) FROM {schema}.{object};");
        let ctx = TemplateObjectContext {
            schema: "dbo".into(),
            object: "x]; DROP TABLE y;--".into(),
            ..TemplateObjectContext::default()
        };
        assert_eq!(
            template.render_for_object(&HashMap::new(), &ctx, DatabaseType::SqlServer),
            "SELECT COUNT(*) FROM [dbo].[x]]; DROP TABLE y;--];"
        );
        let ctx = TemplateObjectContext {
            schema: "public".into(),
            object: "x\"; DROP TABLE y;--".into(),
            ..TemplateObjectContext::default()
        };
        assert_eq!(
            template.render_for_object(&HashMap::new(), &ctx, DatabaseType::Postgres),
            "SELECT COUNT(*) FROM \"public\".\"x\"\"; DROP TABLE y;--\";"
        );
    }

    #[test]
    fn names_with_spaces_and_reserved_words_stay_one_identifier() {
        let template = body_template("SELECT * FROM {schema}.{object}");
        let ctx = TemplateObjectContext {
            schema: "order".into(),
            object: "line items".into(),
            ..TemplateObjectContext::default()
        };
        assert_eq!(
            template.render_for_object(&HashMap::new(), &ctx, DatabaseType::MySql),
            "SELECT * FROM `order`.`line items`"
        );
    }

    #[test]
    fn literal_and_raw_forms_escape_for_their_context() {
        let template = body_template(
            "-- {object:raw}\nSELECT OBJECT_ID({object:literal}), {schema:literal}, {kind}",
        );
        let ctx = TemplateObjectContext {
            schema: String::new(),
            object: "it's\nDROP TABLE t".into(),
            kind: Some(ExplorerObjectKind::View),
            ..TemplateObjectContext::default()
        };
        assert_eq!(
            template.render_for_object(&HashMap::new(), &ctx, DatabaseType::SqlServer),
            "-- it's DROP TABLE t\nSELECT OBJECT_ID(N'it''s\nDROP TABLE t'), N'', view"
        );
        // MySQL literals also double backslashes, so a trailing `\` cannot
        // eat the closing quote.
        let ctx = TemplateObjectContext {
            object: "a\\".into(),
            ..TemplateObjectContext::default()
        };
        let my = body_template("{object:literal}").render_for_object(
            &HashMap::new(),
            &ctx,
            DatabaseType::MySql,
        );
        assert_eq!(my, "'a\\\\'");
    }

    #[test]
    fn substitution_is_single_pass_for_hostile_names() {
        // A name that spells another placeholder (or an SSMS parameter) is
        // written once, quoted, and never expanded again.
        let template = body_template("{schema}.{object} <n,int,5>");
        let ctx = TemplateObjectContext {
            schema: "{object}".into(),
            object: "<n,int,5>".into(),
            ..TemplateObjectContext::default()
        };
        assert_eq!(
            template.render_for_object(&HashMap::new(), &ctx, DatabaseType::SqlServer),
            "[{object}].[<n,int,5>] 5"
        );
    }

    #[test]
    fn unknown_brace_text_is_left_alone() {
        let template = body_template("SELECT '{\"a\": 1}', {objects}, {object:upper}, {kind:raw}");
        let rendered =
            template.render_for_object(&HashMap::new(), &orders_ctx(), DatabaseType::Postgres);
        assert_eq!(
            rendered,
            "SELECT '{\"a\": 1}', {objects}, {object:upper}, {kind:raw}"
        );
    }

    #[test]
    fn explorer_action_matches_listed_kinds_and_empty_means_all() {
        let tables_only =
            action_template(vec![ExplorerObjectKind::Table], ExplorerActionMode::Script);
        assert!(
            tables_only
                .explorer_action_for(ExplorerObjectKind::Table)
                .is_some()
        );
        assert!(
            tables_only
                .explorer_action_for(ExplorerObjectKind::View)
                .is_none()
        );
        let every_kind = action_template(vec![], ExplorerActionMode::Script);
        for kind in ExplorerObjectKind::ALL {
            assert!(every_kind.explorer_action_for(kind).is_some());
        }
        // A template with no spec is never an explorer action.
        let plain = built_in_templates().remove(0);
        assert!(plain.explorer_action.is_none());
        assert!(
            plain
                .explorer_action_for(ExplorerObjectKind::Table)
                .is_none()
        );
    }

    #[test]
    fn explorer_actions_for_filters_in_catalog_order() {
        let a = action_template(vec![], ExplorerActionMode::Script);
        let mut b = action_template(vec![ExplorerObjectKind::View], ExplorerActionMode::Run);
        b.name = "Views only".to_string();
        let mut plain = action_template(vec![], ExplorerActionMode::Script);
        plain.name = "Not an action".to_string();
        plain.explorer_action = None;
        let catalog = vec![a, b, plain];
        let names: Vec<&str> = explorer_actions_for(&catalog, ExplorerObjectKind::View)
            .into_iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, vec!["Row count", "Views only"]);
        assert_eq!(
            explorer_actions_for(&catalog, ExplorerObjectKind::Procedure).len(),
            1
        );
    }

    #[test]
    fn explorer_action_spec_round_trips_and_old_files_default_to_none() {
        let template = action_template(vec![ExplorerObjectKind::Function], ExplorerActionMode::Run);
        let file = UserTemplatesFile::new(vec![template.clone()]);
        let restored = UserTemplatesFile::from_json(&file.to_json());
        assert_eq!(
            restored.templates[0].explorer_action,
            template.explorer_action
        );
        // A catalog written before explorer actions existed loads with no spec.
        let old = r#"{"version":1,"templates":[{"name":"Mine","body":"SELECT 1"}]}"#;
        let file = UserTemplatesFile::from_json(old);
        assert!(file.templates[0].explorer_action.is_none());
    }

    #[test]
    fn explorer_action_mode_defaults_to_script() {
        let spec = ExplorerActionSpec::default();
        assert_eq!(spec.mode, ExplorerActionMode::Script);
        // Deserialize tolerates a spec that names kinds but not the mode.
        let spec: ExplorerActionSpec =
            serde_json::from_str(r#"{"kinds":["Table"]}"#).expect("mode defaults");
        assert_eq!(spec.mode, ExplorerActionMode::Script);
        assert_eq!(spec.kinds, vec![ExplorerObjectKind::Table]);
    }
}
