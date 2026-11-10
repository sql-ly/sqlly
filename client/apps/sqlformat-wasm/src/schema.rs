//! The settings schema the web formatter's Advanced panel is built from.
//!
//! Every layout setting the formatter reads, grouped the way the desktop
//! app's Editor ▸ Formatting pane groups them, with the choices for each
//! enum taken from the formatter's own types (`all()` / `display_title()`),
//! so the page can never offer a value the formatter does not know. The
//! page receives this once as JSON and renders its controls from it.
//!
//! Keys are the persisted `sql-formatting.json` names; settings nested under
//! `keywordGroupCasing` use a dotted key (`keywordGroupCasing.dataTypes`).
//! App-only settings (format triggers, editor indentation, the preset marker
//! and the dialect, which the page has its own control for) are left out.

use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlly_lang::sqlformat::{
    BooleanOperatorPlacement, CaseExpressionStyle, ClauseBodyPlacement, CommaStyle, CommentStyle,
    CteStyle, DdlStyle, DmlStyle, FunctionArgStyle, FunctionCase, IdentifierCase,
    IdentifierQuoting, InListStyle, IndentStyle, JoinLayout, KeywordCase, MajorClausePlacement,
    OnClausePlacement, OrderGroupLayout, Preset, SelectListStyle, SemicolonStyle,
    SqlFormattingPreferences, SubqueryStyle, ValuesStyle,
};

/// One control in the Advanced panel.
#[derive(Debug, Clone)]
pub enum Kind {
    /// A `<select>`: `(value, label)` pairs, value being the serde name.
    Choice(Vec<(String, String)>),
    Toggle,
    /// An integer input. `zero_disables` marks a 0 that means "off" and sits
    /// outside `min..=max` (max line length: 0 = never wrap, else 40–500).
    Number {
        min: i32,
        max: i32,
        zero_disables: bool,
    },
    /// Free text with a length cap.
    Text {
        max_len: usize,
    },
}

#[derive(Debug, Clone)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub hint: Option<&'static str>,
    pub kind: Kind,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub id: &'static str,
    pub label: &'static str,
    pub fields: Vec<Field>,
}

/// Presets the page offers, in its Style menu order. `UppercaseKeywords` is
/// identical to `Default` apart from its marker and `LowercaseKeywords` is
/// Default plus two casing settings, so the page leaves both to the casing
/// controls; `Custom` is a marker with no values of its own.
pub const PAGE_PRESETS: [Preset; 4] = [
    Preset::Default,
    Preset::Compact,
    Preset::Expanded,
    Preset::PreserveExisting,
];

/// Settings in `sql-formatting.json` that the web page deliberately does not
/// expose (see the module docs). Anything else must be in [`groups`]; a test
/// enforces it so a new formatter setting cannot silently miss the page.
pub const NOT_ON_THE_PAGE: [&str; 7] = [
    "preset",
    "dialect",
    "formatAsYouGo",
    "formatOnSave",
    "formatOnPaste",
    "useEditorIndentation",
    // The nested group itself; its members are listed as dotted keys.
    "keywordGroupCasing",
];

fn serde_name<T: Serialize>(v: T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        other => unreachable!("enum did not serialize to a string: {other:?}"),
    }
}

fn choice<T: Serialize + Copy>(all: &[T], title: fn(T) -> &'static str) -> Kind {
    Kind::Choice(
        all.iter()
            .map(|&v| (serde_name(v), title(v).to_string()))
            .collect(),
    )
}

fn field(key: &'static str, label: &'static str, kind: Kind) -> Field {
    Field {
        key,
        label,
        hint: None,
        kind,
    }
}

fn hinted(key: &'static str, label: &'static str, hint: &'static str, kind: Kind) -> Field {
    Field {
        key,
        label,
        hint: Some(hint),
        kind,
    }
}

fn number(min: i32, max: i32) -> Kind {
    Kind::Number {
        min,
        max,
        zero_disables: false,
    }
}

fn keyword_case() -> Kind {
    choice(KeywordCase::all(), KeywordCase::display_title)
}

/// The Advanced panel, group by group. Labels follow the desktop app's
/// Editor ▸ Formatting pane.
pub fn groups() -> Vec<Group> {
    use Kind::Toggle;
    vec![
        Group {
            id: "casing",
            label: "Keyword casing",
            fields: vec![
                field("keywordCase", "Keywords", keyword_case()),
                field(
                    "functionCase",
                    "Function names",
                    choice(FunctionCase::all(), FunctionCase::display_title),
                ),
                hinted(
                    "keywordGroupCasing.enabled",
                    "Separate casing per keyword category",
                    "When on, each category below overrides the keyword casing above.",
                    Toggle,
                ),
                field("keywordGroupCasing.dataTypes", "Data types", keyword_case()),
                field("keywordGroupCasing.functions", "Functions", keyword_case()),
                field(
                    "keywordGroupCasing.keywords",
                    "Other keywords",
                    keyword_case(),
                ),
                field("keywordGroupCasing.clauses", "Clauses", keyword_case()),
                field(
                    "keywordGroupCasing.operators",
                    "Operators (AND, OR, LIKE…)",
                    keyword_case(),
                ),
                field("keywordGroupCasing.joins", "JOINs", keyword_case()),
                field(
                    "keywordGroupCasing.setOperations",
                    "Set operations",
                    keyword_case(),
                ),
                field("keywordGroupCasing.dml", "DML", keyword_case()),
                field("keywordGroupCasing.ddl", "DDL", keyword_case()),
                field(
                    "keywordGroupCasing.transaction",
                    "Transaction",
                    keyword_case(),
                ),
            ],
        },
        Group {
            id: "identifiers",
            label: "Identifiers",
            fields: vec![
                field(
                    "identifierCase",
                    "Identifier case",
                    choice(IdentifierCase::all(), IdentifierCase::display_title),
                ),
                hinted(
                    "identifierQuoting",
                    "Quoting",
                    "Quotes are only removed where the name still resolves to the same object under the dialect's case rules.",
                    choice(IdentifierQuoting::all(), IdentifierQuoting::display_title),
                ),
            ],
        },
        Group {
            id: "clauses",
            label: "Clause layout",
            fields: vec![
                field(
                    "majorClausePlacement",
                    "Major clause placement",
                    choice(
                        MajorClausePlacement::all(),
                        MajorClausePlacement::display_title,
                    ),
                ),
                field(
                    "fromTablePlacement",
                    "Table after FROM",
                    choice(
                        ClauseBodyPlacement::all(),
                        ClauseBodyPlacement::display_title,
                    ),
                ),
                field(
                    "whereConditionPlacement",
                    "First condition after WHERE / HAVING",
                    choice(
                        ClauseBodyPlacement::all(),
                        ClauseBodyPlacement::display_title,
                    ),
                ),
                field("breakAfterSelect", "Break after SELECT", Toggle),
                field(
                    "blankLineBeforeMajorClauses",
                    "Blank line before major clauses",
                    Toggle,
                ),
            ],
        },
        Group {
            id: "select",
            label: "SELECT list",
            fields: vec![
                field(
                    "selectColumnLayout",
                    "Column layout",
                    choice(SelectListStyle::all(), SelectListStyle::display_title),
                ),
                hinted(
                    "inlineColumnThreshold",
                    "Inline column threshold",
                    "Columns at or below this count may share one line. 0–20.",
                    number(0, 20),
                ),
                field("alignColumnAliases", "Align column aliases", Toggle),
                field(
                    "orderGroupLayout",
                    "GROUP BY / ORDER BY layout",
                    choice(OrderGroupLayout::all(), OrderGroupLayout::display_title),
                ),
                field(
                    "commaStyle",
                    "Commas",
                    choice(CommaStyle::all(), CommaStyle::display_title),
                ),
            ],
        },
        Group {
            id: "conditions",
            label: "Conditions and expressions",
            fields: vec![
                field(
                    "booleanOperatorPlacement",
                    "AND / OR placement",
                    choice(
                        BooleanOperatorPlacement::all(),
                        BooleanOperatorPlacement::display_title,
                    ),
                ),
                hinted(
                    "parenthesizeMixedBooleanLogic",
                    "Parenthesize mixed boolean logic",
                    "Adds parentheses around AND runs inside an OR, making the precedence the database already applies visible.",
                    Toggle,
                ),
                field(
                    "inListStyle",
                    "IN list layout",
                    choice(InListStyle::all(), InListStyle::display_title),
                ),
                field(
                    "caseExpressionStyle",
                    "CASE expressions",
                    choice(
                        CaseExpressionStyle::all(),
                        CaseExpressionStyle::display_title,
                    ),
                ),
                field(
                    "spacesAroundOperators",
                    "Spaces around operators (=, <>, +, etc.)",
                    Toggle,
                ),
                field(
                    "alignComparisonOperators",
                    "Align comparison operators",
                    Toggle,
                ),
            ],
        },
        Group {
            id: "joins",
            label: "JOINs",
            fields: vec![
                field(
                    "joinLayout",
                    "Layout",
                    choice(JoinLayout::all(), JoinLayout::display_title),
                ),
                field(
                    "onClausePlacement",
                    "ON clause placement",
                    choice(OnClausePlacement::all(), OnClausePlacement::display_title),
                ),
                field(
                    "multiConditionON",
                    "Multi-condition ON (each condition on its own line)",
                    Toggle,
                ),
                field("indentJoinON", "Indent JOIN ON conditions", Toggle),
            ],
        },
        Group {
            id: "functions",
            label: "Functions",
            fields: vec![
                field(
                    "functionArgStyle",
                    "Argument style",
                    choice(FunctionArgStyle::all(), FunctionArgStyle::display_title),
                ),
                hinted(
                    "spaceBeforeFunctionParentheses",
                    "Space before function parentheses",
                    "MySQL rejects a space after COUNT, SUM and some other built-in functions unless the IGNORE_SPACE SQL mode is on.",
                    Toggle,
                ),
            ],
        },
        Group {
            id: "subqueries",
            label: "Subqueries and CTEs",
            fields: vec![
                field(
                    "subqueryStyle",
                    "Subquery style",
                    choice(SubqueryStyle::all(), SubqueryStyle::display_title),
                ),
                field(
                    "cteStyle",
                    "CTE style",
                    choice(CteStyle::all(), CteStyle::display_title),
                ),
                field("newLineAfterWITH", "New line after WITH", Toggle),
                field("indentNestedQueries", "Indent nested queries", Toggle),
            ],
        },
        Group {
            id: "dml",
            label: "INSERT, UPDATE, DELETE, MERGE",
            fields: vec![
                field(
                    "dmlStyle",
                    "Style",
                    choice(DmlStyle::all(), DmlStyle::display_title),
                ),
                field(
                    "valuesStyle",
                    "VALUES style",
                    choice(ValuesStyle::all(), ValuesStyle::display_title),
                ),
                field(
                    "insertOneColumnPerLine",
                    "INSERT: one column per line",
                    Toggle,
                ),
                field("alignValuesRows", "Align VALUES rows", Toggle),
                field(
                    "alignSetOperators",
                    "Align UPDATE … SET assignments",
                    Toggle,
                ),
                field(
                    "mergeSectionSeparation",
                    "Blank line between MERGE sections",
                    Toggle,
                ),
            ],
        },
        Group {
            id: "ddl",
            label: "CREATE TABLE and other DDL",
            fields: vec![
                field(
                    "ddlStyle",
                    "Style",
                    choice(DdlStyle::all(), DdlStyle::display_title),
                ),
                field("alignDataTypes", "Align data types", Toggle),
                field("alignNullability", "Align nullability", Toggle),
                field(
                    "constraintsAfterColumns",
                    "Constraints after columns",
                    Toggle,
                ),
                field("multilineIndexColumns", "Multiline index columns", Toggle),
            ],
        },
        Group {
            id: "comments",
            label: "Comments",
            fields: vec![
                field(
                    "commentStyle",
                    "Style",
                    choice(CommentStyle::all(), CommentStyle::display_title),
                ),
                field(
                    "keepInlineCommentsInline",
                    "Keep inline comments inline",
                    Toggle,
                ),
                field(
                    "moveTrailingCommentsWithExpression",
                    "Move trailing comments with expression",
                    Toggle,
                ),
                field(
                    "formatInsideBlockComments",
                    "Format inside block comments",
                    Toggle,
                ),
                hinted(
                    "respectControlComments",
                    "Respect control comments",
                    "SQL between -- formatter:off and -- formatter:on is kept exactly as written.",
                    Toggle,
                ),
                hinted(
                    "controlCommentPrefix",
                    "Control comment prefix",
                    "The word before :off / :on in a control comment.",
                    Kind::Text { max_len: 32 },
                ),
            ],
        },
        Group {
            id: "indentation",
            label: "Indentation",
            fields: vec![
                field(
                    "indentStyle",
                    "Indent with",
                    choice(IndentStyle::all(), IndentStyle::display_title),
                ),
                hinted(
                    "indentWidth",
                    "Indent width",
                    "Spaces per level; also the width a tab counts as. 1–8.",
                    number(1, 8),
                ),
                hinted(
                    "continuationIndentSize",
                    "Continuation indent",
                    "Extra indent for a wrapped line. 1–8.",
                    number(1, 8),
                ),
            ],
        },
        Group {
            id: "lines",
            label: "Line length and statements",
            fields: vec![
                hinted(
                    "maxLineLength",
                    "Max line length",
                    "0 disables wrapping entirely; any other value is clamped to 40–500.",
                    Kind::Number {
                        min: 40,
                        max: 500,
                        zero_disables: true,
                    },
                ),
                field("wrapLongLines", "Wrap long lines", Toggle),
                field(
                    "keepShortQueriesOnOneLine",
                    "Keep short queries on one line",
                    Toggle,
                ),
                hinted(
                    "shortQueryMaxLength",
                    "Short query max length",
                    "Queries up to this many characters stay on one line. 0–500.",
                    number(0, 500),
                ),
                hinted(
                    "semicolonStyle",
                    "Semicolon placement",
                    "The formatter never adds or removes semicolons; this only moves them.",
                    choice(SemicolonStyle::all(), SemicolonStyle::display_title),
                ),
                hinted(
                    "maxConsecutiveBlankLines",
                    "Max consecutive blank lines",
                    "Blank lines between statements are kept up to this count. 0–5.",
                    number(0, 5),
                ),
            ],
        },
    ]
}

/// The value of every schema field under `prefs`, by dotted key.
pub fn flatten(prefs: &SqlFormattingPreferences) -> Map<String, Value> {
    let mut out = Map::new();
    if let Ok(Value::Object(map)) = serde_json::to_value(prefs) {
        for (key, value) in map {
            match value {
                Value::Object(inner) => {
                    for (k, v) in inner {
                        out.insert(format!("{key}.{k}"), v);
                    }
                }
                v => {
                    out.insert(key, v);
                }
            }
        }
    }
    out
}

/// The schema as the page consumes it:
/// `{ groups: [{id, label, fields: [{key, label, hint?, type, …}]}],
///    presets: { default: {key: value, …}, compact: {…}, … } }`.
pub fn schema_json() -> Value {
    let keys: Vec<&'static str> = groups()
        .iter()
        .flat_map(|g| g.fields.iter().map(|f| f.key))
        .collect();
    let groups: Vec<Value> = groups()
        .into_iter()
        .map(|g| {
            let fields: Vec<Value> = g.fields.into_iter().map(field_json).collect();
            json!({ "id": g.id, "label": g.label, "fields": fields })
        })
        .collect();
    let mut presets = Map::new();
    for preset in PAGE_PRESETS {
        let all = flatten(&SqlFormattingPreferences::for_preset(preset));
        let values: Map<String, Value> = keys
            .iter()
            .filter_map(|k| all.get(*k).map(|v| ((*k).to_string(), v.clone())))
            .collect();
        presets.insert(serde_name(preset), Value::Object(values));
    }
    json!({ "groups": groups, "presets": presets })
}

fn field_json(f: Field) -> Value {
    let mut v = json!({ "key": f.key, "label": f.label });
    let obj = v.as_object_mut().expect("object literal");
    if let Some(hint) = f.hint {
        obj.insert("hint".into(), hint.into());
    }
    match f.kind {
        Kind::Choice(options) => {
            obj.insert("type".into(), "choice".into());
            let options: Vec<Value> = options
                .into_iter()
                .map(|(value, label)| json!({ "value": value, "label": label }))
                .collect();
            obj.insert("options".into(), options.into());
        }
        Kind::Toggle => {
            obj.insert("type".into(), "toggle".into());
        }
        Kind::Number {
            min,
            max,
            zero_disables,
        } => {
            obj.insert("type".into(), "number".into());
            obj.insert("min".into(), min.into());
            obj.insert("max".into(), max.into());
            if zero_disables {
                obj.insert("zeroDisables".into(), true.into());
            }
        }
        Kind::Text { max_len } => {
            obj.insert("type".into(), "text".into());
            obj.insert("maxLength".into(), max_len.into());
        }
    }
    v
}
