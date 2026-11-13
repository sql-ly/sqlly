//! SQL formatter preference types, extracted from the GPUI app so the
//! formatter (and the LSP's `textDocument/formatting`) can live down-stack.
//! Serde shapes are frozen: profiles written by earlier app versions load
//! unchanged.

use serde::{Deserialize, Serialize};

/// Shared by `EditorPreferences` and `SqlFormattingPreferences`. The Swift port
/// declared a second, nested `SQLFormatterPreferences.IndentStyle`, but it is
/// this enum case-for-case and tag-for-tag ("tabs"/"spaces"), so a separate type
/// would only be two names for one concept. The differing *defaults* (editor
/// tabs, formatter spaces) are per-field, not properties of the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum IndentStyle {
    #[default]
    Tabs,
    Spaces,
}

impl IndentStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            IndentStyle::Tabs => "Tabs",
            IndentStyle::Spaces => "Spaces",
        }
    }

    pub fn all() -> &'static [IndentStyle] {
        &[IndentStyle::Tabs, IndentStyle::Spaces]
    }
}

// ============================================================================
// SQL Formatting Preferences
// ============================================================================
//
// Port of `SQLFormatterPreferences.swift`. There is no formatter engine in
// either the Swift app or here — this is a pure data model so the Preferences
// The user-facing formatter settings. Read by the SQL formatter below and
// by the LSP's textDocument/formatting; persisted by the app as
// `sql-formatting.json`.
//

/// Named bundles of formatter settings the user can apply in one click.
/// Selecting a preset overwrites every individual setting; the preset is then
/// stored so the UI can show it as active until any single setting changes (at
/// which point the UI moves it to `Custom`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Preset {
    #[default]
    Default,
    Compact,
    Expanded,
    UppercaseKeywords,
    LowercaseKeywords,
    PreserveExisting,
    Custom,
}

impl Preset {
    pub fn display_title(self) -> &'static str {
        match self {
            Preset::Default => "Default",
            Preset::Compact => "Compact",
            Preset::Expanded => "Expanded",
            Preset::UppercaseKeywords => "Uppercase Keywords",
            Preset::LowercaseKeywords => "Lowercase Keywords",
            Preset::PreserveExisting => "Preserve Existing",
            Preset::Custom => "Custom",
        }
    }

    pub fn all() -> &'static [Preset] {
        &[
            Preset::Default,
            Preset::Compact,
            Preset::Expanded,
            Preset::UppercaseKeywords,
            Preset::LowercaseKeywords,
            Preset::PreserveExisting,
            Preset::Custom,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum KeywordCase {
    #[default]
    Uppercase,
    Lowercase,
    TitleCase,
    MatchSurroundingStyle,
    Preserve,
}

impl KeywordCase {
    pub fn display_title(self) -> &'static str {
        match self {
            KeywordCase::Uppercase => "UPPERCASE",
            KeywordCase::Lowercase => "lowercase",
            KeywordCase::TitleCase => "Title Case",
            KeywordCase::MatchSurroundingStyle => "Match Surrounding Style",
            KeywordCase::Preserve => "Preserve",
        }
    }

    pub fn all() -> &'static [KeywordCase] {
        &[
            KeywordCase::Uppercase,
            KeywordCase::Lowercase,
            KeywordCase::TitleCase,
            KeywordCase::MatchSurroundingStyle,
            KeywordCase::Preserve,
        ]
    }
}

/// Per-category keyword casing. When `enabled`, each category overrides the
/// global `keyword_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeywordGroupCasing {
    pub enabled: bool,
    pub data_types: KeywordCase,
    pub functions: KeywordCase,
    pub keywords: KeywordCase,
    pub clauses: KeywordCase,
    pub operators: KeywordCase,
    pub joins: KeywordCase,
    pub set_operations: KeywordCase,
    pub dml: KeywordCase,
    pub ddl: KeywordCase,
    pub transaction: KeywordCase,
}

impl Default for KeywordGroupCasing {
    fn default() -> Self {
        Self {
            enabled: false,
            data_types: KeywordCase::Uppercase,
            functions: KeywordCase::Uppercase,
            keywords: KeywordCase::Uppercase,
            clauses: KeywordCase::Uppercase,
            operators: KeywordCase::Uppercase,
            joins: KeywordCase::Uppercase,
            set_operations: KeywordCase::Uppercase,
            dml: KeywordCase::Uppercase,
            ddl: KeywordCase::Uppercase,
            transaction: KeywordCase::Uppercase,
        }
    }
}

impl KeywordGroupCasing {
    /// UI iteration order and labels. Stands in for the Swift
    /// `keywordGroupCategories` key-path table; `category`/`set_category` index
    /// into this list.
    pub const CATEGORY_LABELS: [&'static str; 10] = [
        "Data types",
        "Functions",
        "Keywords",
        "Clauses",
        "Operators",
        "JOINs",
        "Set operations",
        "DML",
        "DDL",
        "Transaction",
    ];

    /// Every category set to `Preserve` — the shape the `PreserveExisting`
    /// preset needs.
    fn all_preserve() -> Self {
        Self {
            enabled: false,
            data_types: KeywordCase::Preserve,
            functions: KeywordCase::Preserve,
            keywords: KeywordCase::Preserve,
            clauses: KeywordCase::Preserve,
            operators: KeywordCase::Preserve,
            joins: KeywordCase::Preserve,
            set_operations: KeywordCase::Preserve,
            dml: KeywordCase::Preserve,
            ddl: KeywordCase::Preserve,
            transaction: KeywordCase::Preserve,
        }
    }

    /// Reads the category at `CATEGORY_LABELS[index]`; `None` when out of range.
    pub fn category(&self, index: usize) -> Option<KeywordCase> {
        Some(match index {
            0 => self.data_types,
            1 => self.functions,
            2 => self.keywords,
            3 => self.clauses,
            4 => self.operators,
            5 => self.joins,
            6 => self.set_operations,
            7 => self.dml,
            8 => self.ddl,
            9 => self.transaction,
            _ => return None,
        })
    }

    /// Writes the category at `CATEGORY_LABELS[index]`; out-of-range is a no-op.
    pub fn set_category(&mut self, index: usize, value: KeywordCase) {
        match index {
            0 => self.data_types = value,
            1 => self.functions = value,
            2 => self.keywords = value,
            3 => self.clauses = value,
            4 => self.operators = value,
            5 => self.joins = value,
            6 => self.set_operations = value,
            7 => self.dml = value,
            8 => self.ddl = value,
            9 => self.transaction = value,
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum IdentifierCase {
    #[default]
    Preserve,
    Lowercase,
    Uppercase,
    PascalCase,
}

impl IdentifierCase {
    pub fn display_title(self) -> &'static str {
        match self {
            IdentifierCase::Preserve => "Preserve",
            IdentifierCase::Lowercase => "lowercase",
            IdentifierCase::Uppercase => "UPPERCASE",
            IdentifierCase::PascalCase => "PascalCase",
        }
    }

    pub fn all() -> &'static [IdentifierCase] {
        &[
            IdentifierCase::Preserve,
            IdentifierCase::Lowercase,
            IdentifierCase::Uppercase,
            IdentifierCase::PascalCase,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum IdentifierQuoting {
    #[default]
    PreserveExisting,
    QuoteOnlyWhenRequired,
    AlwaysQuote,
    RemoveUnnecessaryQuotes,
}

impl IdentifierQuoting {
    pub fn display_title(self) -> &'static str {
        match self {
            IdentifierQuoting::PreserveExisting => "Preserve Existing",
            IdentifierQuoting::QuoteOnlyWhenRequired => "Quote Only When Required",
            IdentifierQuoting::AlwaysQuote => "Always Quote",
            IdentifierQuoting::RemoveUnnecessaryQuotes => "Remove Unnecessary Quotes",
        }
    }

    pub fn all() -> &'static [IdentifierQuoting] {
        &[
            IdentifierQuoting::PreserveExisting,
            IdentifierQuoting::QuoteOnlyWhenRequired,
            IdentifierQuoting::AlwaysQuote,
            IdentifierQuoting::RemoveUnnecessaryQuotes,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum MajorClausePlacement {
    #[default]
    EachOnNewLine,
    PreserveExisting,
    CompactWhenShort,
}

impl MajorClausePlacement {
    pub fn display_title(self) -> &'static str {
        match self {
            MajorClausePlacement::EachOnNewLine => "Each Major Clause On New Line",
            MajorClausePlacement::PreserveExisting => "Preserve Existing",
            MajorClausePlacement::CompactWhenShort => "Compact When Short",
        }
    }

    pub fn all() -> &'static [MajorClausePlacement] {
        &[
            MajorClausePlacement::EachOnNewLine,
            MajorClausePlacement::PreserveExisting,
            MajorClausePlacement::CompactWhenShort,
        ]
    }
}

/// Where the body of a FROM / WHERE / HAVING clause starts: on a new line
/// indented under the keyword (the default), or on the keyword's own line.
/// gpui-first addition; see [`CaseExpressionStyle`] on cross-app
/// compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ClauseBodyPlacement {
    #[default]
    NewLineIndented,
    SameLine,
}

impl ClauseBodyPlacement {
    /// English title for hosts without a catalog (the browser formatter). The
    /// desktop app labels its dropdown from the translation catalog instead,
    /// and persists the serde name, never this text.
    pub fn display_title(self) -> &'static str {
        match self {
            ClauseBodyPlacement::NewLineIndented => "New line, indented",
            ClauseBodyPlacement::SameLine => "Same line as the keyword",
        }
    }

    pub fn all() -> &'static [ClauseBodyPlacement] {
        &[
            ClauseBodyPlacement::NewLineIndented,
            ClauseBodyPlacement::SameLine,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SelectListStyle {
    #[default]
    PreserveExisting,
    OneLineWhenShort,
    Stacked,
    Wrap,
}

impl SelectListStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            SelectListStyle::PreserveExisting => "Preserve Existing",
            SelectListStyle::OneLineWhenShort => "One Line When Short",
            SelectListStyle::Stacked => "Stacked (each column on its own line)",
            SelectListStyle::Wrap => "Wrap at line length",
        }
    }

    pub fn all() -> &'static [SelectListStyle] {
        &[
            SelectListStyle::PreserveExisting,
            SelectListStyle::OneLineWhenShort,
            SelectListStyle::Stacked,
            SelectListStyle::Wrap,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum CommaStyle {
    #[default]
    Trailing,
    Leading,
    PreserveExisting,
}

impl CommaStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            CommaStyle::Trailing => "Trailing (column,)",
            CommaStyle::Leading => "Leading (,column)",
            CommaStyle::PreserveExisting => "Preserve Existing",
        }
    }

    pub fn all() -> &'static [CommaStyle] {
        &[
            CommaStyle::Trailing,
            CommaStyle::Leading,
            CommaStyle::PreserveExisting,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum BooleanOperatorPlacement {
    #[default]
    LineLeading,
    LineTrailing,
    PreserveExisting,
}

impl BooleanOperatorPlacement {
    pub fn display_title(self) -> &'static str {
        match self {
            BooleanOperatorPlacement::LineLeading => "Line Leading (AND/OR at start of line)",
            BooleanOperatorPlacement::LineTrailing => "Line Trailing (AND/OR at end of line)",
            BooleanOperatorPlacement::PreserveExisting => "Preserve Existing",
        }
    }

    pub fn all() -> &'static [BooleanOperatorPlacement] {
        &[
            BooleanOperatorPlacement::LineLeading,
            BooleanOperatorPlacement::LineTrailing,
            BooleanOperatorPlacement::PreserveExisting,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum JoinLayout {
    #[default]
    Aligned,
    Stacked,
    Compact,
}

impl JoinLayout {
    pub fn display_title(self) -> &'static str {
        match self {
            JoinLayout::Aligned => "Aligned (JOIN keyword aligned)",
            JoinLayout::Stacked => "Stacked (each JOIN on a new line)",
            JoinLayout::Compact => "Compact (fit on one line)",
        }
    }

    pub fn all() -> &'static [JoinLayout] {
        &[
            JoinLayout::Aligned,
            JoinLayout::Stacked,
            JoinLayout::Compact,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum OnClausePlacement {
    #[default]
    AfterJoin,
    OnNewLine,
    PreserveExisting,
}

impl OnClausePlacement {
    pub fn display_title(self) -> &'static str {
        match self {
            OnClausePlacement::AfterJoin => "After JOIN keyword",
            OnClausePlacement::OnNewLine => "On a new line",
            OnClausePlacement::PreserveExisting => "Preserve Existing",
        }
    }

    pub fn all() -> &'static [OnClausePlacement] {
        &[
            OnClausePlacement::AfterJoin,
            OnClausePlacement::OnNewLine,
            OnClausePlacement::PreserveExisting,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum FunctionCase {
    #[default]
    Preserve,
    Lowercase,
    Uppercase,
}

impl FunctionCase {
    pub fn display_title(self) -> &'static str {
        match self {
            FunctionCase::Preserve => "Preserve",
            FunctionCase::Lowercase => "lowercase",
            FunctionCase::Uppercase => "UPPERCASE",
        }
    }

    pub fn all() -> &'static [FunctionCase] {
        &[
            FunctionCase::Preserve,
            FunctionCase::Lowercase,
            FunctionCase::Uppercase,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum FunctionArgStyle {
    #[default]
    Inline,
    Stacked,
    Wrap,
}

impl FunctionArgStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            FunctionArgStyle::Inline => "Inline (args on one line)",
            FunctionArgStyle::Stacked => "Stacked (each arg on its own line)",
            FunctionArgStyle::Wrap => "Wrap at line length",
        }
    }

    pub fn all() -> &'static [FunctionArgStyle] {
        &[
            FunctionArgStyle::Inline,
            FunctionArgStyle::Stacked,
            FunctionArgStyle::Wrap,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SubqueryStyle {
    #[default]
    Indented,
    Aligned,
    Expanded,
}

impl SubqueryStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            SubqueryStyle::Indented => "Indented (under parent)",
            SubqueryStyle::Aligned => "Aligned (with parent)",
            SubqueryStyle::Expanded => "Expanded (each part on a new line)",
        }
    }

    pub fn all() -> &'static [SubqueryStyle] {
        &[
            SubqueryStyle::Indented,
            SubqueryStyle::Aligned,
            SubqueryStyle::Expanded,
        ]
    }
}

/// Swift `CTEStyle`; renamed to Rust's acronym casing. The serde tags are
/// unchanged (`inline` / `stacked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum CteStyle {
    Inline,
    #[default]
    Stacked,
}

impl CteStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            CteStyle::Inline => "Inline (CTE body on one line)",
            CteStyle::Stacked => "Stacked (CTE body on its own lines)",
        }
    }

    pub fn all() -> &'static [CteStyle] {
        &[CteStyle::Inline, CteStyle::Stacked]
    }
}

/// Swift `DMLStyle`; see `CteStyle` on the rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DmlStyle {
    #[default]
    Standard,
    Compact,
    Expanded,
}

impl DmlStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            DmlStyle::Standard => "Standard (each clause on a new line)",
            DmlStyle::Compact => "Compact (fit on one line)",
            DmlStyle::Expanded => "Expanded (each column/value on its own line)",
        }
    }

    pub fn all() -> &'static [DmlStyle] {
        &[DmlStyle::Standard, DmlStyle::Compact, DmlStyle::Expanded]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ValuesStyle {
    Inline,
    #[default]
    Stacked,
}

impl ValuesStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            ValuesStyle::Inline => "Inline (VALUES (1, 2), (3, 4))",
            ValuesStyle::Stacked => "Stacked (each tuple on its own line)",
        }
    }

    pub fn all() -> &'static [ValuesStyle] {
        &[ValuesStyle::Inline, ValuesStyle::Stacked]
    }
}

/// Swift `DDLStyle`; see `CteStyle` on the rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DdlStyle {
    #[default]
    Standard,
    Compact,
    Expanded,
}

impl DdlStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            DdlStyle::Standard => "Standard (each column definition on its own line)",
            DdlStyle::Compact => "Compact (fit on one line)",
            DdlStyle::Expanded => "Expanded (each constraint on its own line)",
        }
    }

    pub fn all() -> &'static [DdlStyle] {
        &[DdlStyle::Standard, DdlStyle::Compact, DdlStyle::Expanded]
    }
}

/// Deliberately has no "Remove" option — the formatter must never drop a
/// user's comment (the Swift suite asserts this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum CommentStyle {
    #[default]
    Preserve,
    Align,
}

impl CommentStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            CommentStyle::Preserve => "Preserve (keep as-is)",
            CommentStyle::Align => "Align (align end-of-line comments)",
        }
    }

    pub fn all() -> &'static [CommentStyle] {
        &[CommentStyle::Preserve, CommentStyle::Align]
    }
}

/// Layout of CASE expressions. gpui-first addition (no Swift counterpart);
/// absent keys default, unknown keys are ignored by the Swift decoder, so
/// profiles stay interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum CaseExpressionStyle {
    #[default]
    Stacked,
    Compact,
    PreserveExisting,
}

impl CaseExpressionStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            CaseExpressionStyle::Stacked => "Stacked (each WHEN/THEN on its own line)",
            CaseExpressionStyle::Compact => "Compact (one line when short)",
            CaseExpressionStyle::PreserveExisting => "Preserve Existing",
        }
    }

    pub fn all() -> &'static [CaseExpressionStyle] {
        &[
            CaseExpressionStyle::Stacked,
            CaseExpressionStyle::Compact,
            CaseExpressionStyle::PreserveExisting,
        ]
    }
}

/// Layout of `IN (…)` value lists. gpui-first addition; see
/// [`CaseExpressionStyle`] on cross-app compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum InListStyle {
    #[default]
    Wrap,
    Inline,
    Stacked,
    PreserveExisting,
}

impl InListStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            InListStyle::Wrap => "Wrap at line length",
            InListStyle::Inline => "Inline (all values on one line)",
            InListStyle::Stacked => "Stacked (each value on its own line)",
            InListStyle::PreserveExisting => "Preserve Existing",
        }
    }

    pub fn all() -> &'static [InListStyle] {
        &[
            InListStyle::Wrap,
            InListStyle::Inline,
            InListStyle::Stacked,
            InListStyle::PreserveExisting,
        ]
    }
}

/// Layout of GROUP BY / ORDER BY / PARTITION BY column lists. gpui-first
/// addition; see [`CaseExpressionStyle`] on cross-app compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum OrderGroupLayout {
    #[default]
    MatchSelectList,
    Inline,
    Stacked,
    PreserveExisting,
}

impl OrderGroupLayout {
    pub fn display_title(self) -> &'static str {
        match self {
            OrderGroupLayout::MatchSelectList => "Match SELECT list layout",
            OrderGroupLayout::Inline => "Inline (all columns on one line)",
            OrderGroupLayout::Stacked => "Stacked (each column on its own line)",
            OrderGroupLayout::PreserveExisting => "Preserve Existing",
        }
    }

    pub fn all() -> &'static [OrderGroupLayout] {
        &[
            OrderGroupLayout::MatchSelectList,
            OrderGroupLayout::Inline,
            OrderGroupLayout::Stacked,
            OrderGroupLayout::PreserveExisting,
        ]
    }
}

/// Where the terminating semicolon lands. The formatter never adds or removes
/// semicolons — this only moves ones the user already wrote. gpui-first
/// addition; see [`CaseExpressionStyle`] on cross-app compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SemicolonStyle {
    #[default]
    EndOfStatement,
    OwnLine,
}

impl SemicolonStyle {
    pub fn display_title(self) -> &'static str {
        match self {
            SemicolonStyle::EndOfStatement => "End of last line",
            SemicolonStyle::OwnLine => "On its own line",
        }
    }

    pub fn all() -> &'static [SemicolonStyle] {
        &[SemicolonStyle::EndOfStatement, SemicolonStyle::OwnLine]
    }
}

/// Swift `SQLDialect`; see `CteStyle` on the rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SqlDialect {
    #[default]
    AutoDetect,
    Postgresql,
    Mysql,
    Sqlite,
    Tsql,
    Oracle,
    Bigquery,
    Snowflake,
    Redshift,
}

impl SqlDialect {
    pub fn display_title(self) -> &'static str {
        match self {
            SqlDialect::AutoDetect => "Auto Detect",
            SqlDialect::Postgresql => "PostgreSQL",
            SqlDialect::Mysql => "MySQL",
            SqlDialect::Sqlite => "SQLite",
            SqlDialect::Tsql => "T-SQL (SQL Server)",
            SqlDialect::Oracle => "Oracle",
            SqlDialect::Bigquery => "BigQuery",
            SqlDialect::Snowflake => "Snowflake",
            SqlDialect::Redshift => "Redshift",
        }
    }

    pub fn all() -> &'static [SqlDialect] {
        &[
            SqlDialect::AutoDetect,
            SqlDialect::Postgresql,
            SqlDialect::Mysql,
            SqlDialect::Sqlite,
            SqlDialect::Tsql,
            SqlDialect::Oracle,
            SqlDialect::Bigquery,
            SqlDialect::Snowflake,
            SqlDialect::Redshift,
        ]
    }
}

/// User-facing SQL formatter settings, persisted to `sql-formatting.json`.
///
/// The `*_raw` fields are the unvalidated on-disk values; read them through the
/// matching accessor (`indent_width()`, `max_line_length()`, …) which applies
/// the same clamps the Swift `clamped*` properties did — a hand-edited prefs
/// file must not be able to hand a formatter an indent of 999.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SqlFormattingPreferences {
    // §1 Preset
    pub preset: Preset,

    // §2 Keyword casing
    pub keyword_case: KeywordCase,

    // §3 Keyword groups
    pub keyword_group_casing: KeywordGroupCasing,

    // §4 Identifier formatting
    pub identifier_case: IdentifierCase,
    pub identifier_quoting: IdentifierQuoting,

    // §5 Clause layout
    pub major_clause_placement: MajorClausePlacement,
    pub break_after_select: bool,
    pub blank_line_before_major_clauses: bool,
    /// FROM's first table: own indented line, or on the FROM line. JOINs
    /// follow it (they indent with the table under `NewLineIndented`).
    pub from_table_placement: ClauseBodyPlacement,
    /// WHERE's / HAVING's first condition: own indented line (aligned with
    /// the AND/OR lines that follow), or on the keyword line.
    pub where_condition_placement: ClauseBodyPlacement,

    // §6 SELECT list
    pub select_column_layout: SelectListStyle,
    #[serde(rename = "inlineColumnThreshold")]
    pub inline_column_threshold_raw: i32,

    // §7 Comma style
    pub comma_style: CommaStyle,

    // §8 Boolean conditions
    pub boolean_operator_placement: BooleanOperatorPlacement,
    pub parenthesize_mixed_boolean_logic: bool,

    // §9 Operators
    pub spaces_around_operators: bool,
    pub align_comparison_operators: bool,

    // §10 JOINs
    pub join_layout: JoinLayout,
    pub on_clause_placement: OnClausePlacement,
    #[serde(rename = "multiConditionON")]
    pub multi_condition_on: bool,

    // §11 Functions
    pub function_case: FunctionCase,
    pub space_before_function_parentheses: bool,
    pub function_arg_style: FunctionArgStyle,

    // §12 Subqueries / CTEs
    pub subquery_style: SubqueryStyle,
    pub cte_style: CteStyle,
    #[serde(rename = "newLineAfterWITH")]
    pub new_line_after_with: bool,

    // §13 DML
    pub dml_style: DmlStyle,
    pub values_style: ValuesStyle,
    pub insert_one_column_per_line: bool,
    pub align_values_rows: bool,
    pub align_set_operators: bool,
    pub merge_section_separation: bool,

    // §14 DDL
    pub ddl_style: DdlStyle,
    pub align_data_types: bool,
    pub align_nullability: bool,
    pub constraints_after_columns: bool,
    pub multiline_index_columns: bool,

    // §15 Comments
    pub comment_style: CommentStyle,
    pub keep_inline_comments_inline: bool,
    pub move_trailing_comments_with_expression: bool,
    pub format_inside_block_comments: bool,

    // §16 Formatter control comments
    pub respect_control_comments: bool,
    pub control_comment_prefix: String,

    // §17 Format triggers
    /// Reformat the statement under the caret each time Enter is pressed.
    /// When off, Format SQL (a rebindable shortcut) formats on demand.
    pub format_as_you_go: bool,
    pub format_on_save: bool,
    pub format_on_paste: bool,

    // §18 Indentation
    pub use_editor_indentation: bool,
    pub indent_style: IndentStyle,
    #[serde(rename = "indentWidth")]
    pub indent_width_raw: i32,
    #[serde(rename = "continuationIndentSize")]
    pub continuation_indent_size_raw: i32,
    pub indent_nested_queries: bool,
    #[serde(rename = "indentJoinON")]
    pub indent_join_on: bool,

    // §19 Line length
    #[serde(rename = "maxLineLength")]
    pub max_line_length_raw: i32,
    pub wrap_long_lines: bool,
    pub keep_short_queries_on_one_line: bool,
    #[serde(rename = "shortQueryMaxLength")]
    pub short_query_max_length_raw: i32,

    // §20 Dialect. The on-disk key is `dialect`, not `sqlDialect` — the Swift
    // profile contract, asserted by a test below.
    pub dialect: SqlDialect,

    // §21 Extended layout — gpui-first additions with no Swift counterpart.
    // The Swift decoder ignores unknown keys and these default when absent,
    // so profiles written by either app still load in the other.
    pub case_expression_style: CaseExpressionStyle,
    pub in_list_style: InListStyle,
    pub order_group_layout: OrderGroupLayout,
    pub semicolon_style: SemicolonStyle,
    #[serde(rename = "maxConsecutiveBlankLines")]
    pub max_consecutive_blank_lines_raw: i32,
    pub align_column_aliases: bool,
}

impl Default for SqlFormattingPreferences {
    fn default() -> Self {
        Self {
            preset: Preset::Default,
            keyword_case: KeywordCase::Uppercase,
            keyword_group_casing: KeywordGroupCasing::default(),
            identifier_case: IdentifierCase::Preserve,
            identifier_quoting: IdentifierQuoting::PreserveExisting,
            major_clause_placement: MajorClausePlacement::EachOnNewLine,
            break_after_select: true,
            blank_line_before_major_clauses: false,
            from_table_placement: ClauseBodyPlacement::NewLineIndented,
            where_condition_placement: ClauseBodyPlacement::NewLineIndented,
            select_column_layout: SelectListStyle::Stacked,
            inline_column_threshold_raw: 3,
            comma_style: CommaStyle::Trailing,
            boolean_operator_placement: BooleanOperatorPlacement::LineLeading,
            parenthesize_mixed_boolean_logic: false,
            spaces_around_operators: true,
            align_comparison_operators: false,
            join_layout: JoinLayout::Aligned,
            on_clause_placement: OnClausePlacement::AfterJoin,
            multi_condition_on: false,
            function_case: FunctionCase::Uppercase,
            space_before_function_parentheses: false,
            function_arg_style: FunctionArgStyle::Inline,
            subquery_style: SubqueryStyle::Indented,
            cte_style: CteStyle::Stacked,
            new_line_after_with: true,
            dml_style: DmlStyle::Standard,
            values_style: ValuesStyle::Stacked,
            insert_one_column_per_line: false,
            align_values_rows: false,
            align_set_operators: false,
            merge_section_separation: false,
            ddl_style: DdlStyle::Standard,
            align_data_types: false,
            align_nullability: false,
            constraints_after_columns: false,
            multiline_index_columns: false,
            comment_style: CommentStyle::Preserve,
            keep_inline_comments_inline: true,
            move_trailing_comments_with_expression: false,
            format_inside_block_comments: false,
            respect_control_comments: true,
            control_comment_prefix: "formatter".to_string(),
            format_as_you_go: false,
            format_on_save: false,
            format_on_paste: false,
            use_editor_indentation: true,
            indent_style: IndentStyle::Spaces,
            indent_width_raw: 4,
            continuation_indent_size_raw: 4,
            indent_nested_queries: true,
            indent_join_on: true,
            max_line_length_raw: 120,
            wrap_long_lines: true,
            keep_short_queries_on_one_line: true,
            short_query_max_length_raw: 80,
            dialect: SqlDialect::AutoDetect,
            case_expression_style: CaseExpressionStyle::Stacked,
            in_list_style: InListStyle::Wrap,
            order_group_layout: OrderGroupLayout::MatchSelectList,
            semicolon_style: SemicolonStyle::EndOfStatement,
            max_consecutive_blank_lines_raw: 1,
            align_column_aliases: false,
        }
    }
}

impl SqlFormattingPreferences {
    /// The full settings for a named preset. Applying a preset in the UI
    /// overwrites every individual setting at once.
    ///
    /// Each arm states only its deltas from `default()`; the Swift source spells
    /// all 54 fields out per preset, but the resulting values are identical and
    /// the deltas are what the presets actually mean.
    pub fn for_preset(preset: Preset) -> Self {
        match preset {
            // `Custom` is not a bundle of settings — it is the marker the UI
            // sets once the user edits any single field, so it has no values of
            // its own and falls back to the defaults.
            Preset::Default | Preset::Custom => Self::default(),
            Preset::Compact => Self {
                preset: Preset::Compact,
                major_clause_placement: MajorClausePlacement::CompactWhenShort,
                break_after_select: false,
                from_table_placement: ClauseBodyPlacement::SameLine,
                where_condition_placement: ClauseBodyPlacement::SameLine,
                select_column_layout: SelectListStyle::OneLineWhenShort,
                boolean_operator_placement: BooleanOperatorPlacement::LineTrailing,
                join_layout: JoinLayout::Compact,
                subquery_style: SubqueryStyle::Aligned,
                cte_style: CteStyle::Inline,
                new_line_after_with: false,
                dml_style: DmlStyle::Compact,
                values_style: ValuesStyle::Inline,
                ddl_style: DdlStyle::Compact,
                indent_style: IndentStyle::Tabs,
                indent_width_raw: 2,
                continuation_indent_size_raw: 2,
                indent_nested_queries: false,
                indent_join_on: false,
                wrap_long_lines: false,
                short_query_max_length_raw: 100,
                case_expression_style: CaseExpressionStyle::Compact,
                in_list_style: InListStyle::Inline,
                order_group_layout: OrderGroupLayout::Inline,
                max_consecutive_blank_lines_raw: 0,
                ..Self::default()
            },
            Preset::Expanded => Self {
                preset: Preset::Expanded,
                blank_line_before_major_clauses: true,
                inline_column_threshold_raw: 1,
                comma_style: CommaStyle::Leading,
                parenthesize_mixed_boolean_logic: true,
                align_comparison_operators: true,
                join_layout: JoinLayout::Stacked,
                on_clause_placement: OnClausePlacement::OnNewLine,
                multi_condition_on: true,
                function_arg_style: FunctionArgStyle::Stacked,
                subquery_style: SubqueryStyle::Expanded,
                dml_style: DmlStyle::Expanded,
                insert_one_column_per_line: true,
                align_values_rows: true,
                align_set_operators: true,
                merge_section_separation: true,
                ddl_style: DdlStyle::Expanded,
                align_data_types: true,
                align_nullability: true,
                constraints_after_columns: true,
                multiline_index_columns: true,
                comment_style: CommentStyle::Align,
                move_trailing_comments_with_expression: true,
                format_inside_block_comments: true,
                use_editor_indentation: false,
                continuation_indent_size_raw: 8,
                max_line_length_raw: 100,
                keep_short_queries_on_one_line: false,
                short_query_max_length_raw: 60,
                in_list_style: InListStyle::Stacked,
                order_group_layout: OrderGroupLayout::Stacked,
                align_column_aliases: true,
                ..Self::default()
            },
            // Identical to Default apart from the marker: the defaults already
            // uppercase keywords. It exists so the dropdown can say so.
            Preset::UppercaseKeywords => Self {
                preset: Preset::UppercaseKeywords,
                ..Self::default()
            },
            Preset::LowercaseKeywords => Self {
                preset: Preset::LowercaseKeywords,
                keyword_case: KeywordCase::Lowercase,
                function_case: FunctionCase::Lowercase,
                ..Self::default()
            },
            Preset::PreserveExisting => Self {
                preset: Preset::PreserveExisting,
                keyword_case: KeywordCase::Preserve,
                keyword_group_casing: KeywordGroupCasing::all_preserve(),
                major_clause_placement: MajorClausePlacement::PreserveExisting,
                break_after_select: false,
                from_table_placement: ClauseBodyPlacement::SameLine,
                where_condition_placement: ClauseBodyPlacement::SameLine,
                select_column_layout: SelectListStyle::PreserveExisting,
                comma_style: CommaStyle::PreserveExisting,
                boolean_operator_placement: BooleanOperatorPlacement::PreserveExisting,
                on_clause_placement: OnClausePlacement::PreserveExisting,
                function_case: FunctionCase::Preserve,
                new_line_after_with: false,
                indent_nested_queries: false,
                indent_join_on: false,
                max_line_length_raw: 0,
                wrap_long_lines: false,
                keep_short_queries_on_one_line: false,
                case_expression_style: CaseExpressionStyle::PreserveExisting,
                in_list_style: InListStyle::PreserveExisting,
                order_group_layout: OrderGroupLayout::PreserveExisting,
                max_consecutive_blank_lines_raw: 3,
                ..Self::default()
            },
        }
    }

    pub fn indent_width(&self) -> i32 {
        self.indent_width_raw.clamp(1, 8)
    }

    pub fn continuation_indent_size(&self) -> i32 {
        self.continuation_indent_size_raw.clamp(1, 8)
    }

    /// 0 means "never wrap" and is passed through; any other value is pulled
    /// into 40..=500 so a nonsense width can't produce one-character lines.
    pub fn max_line_length(&self) -> i32 {
        if self.max_line_length_raw == 0 {
            0
        } else {
            self.max_line_length_raw.clamp(40, 500)
        }
    }

    pub fn inline_column_threshold(&self) -> i32 {
        self.inline_column_threshold_raw.clamp(0, 20)
    }

    pub fn short_query_max_length(&self) -> i32 {
        self.short_query_max_length_raw.clamp(0, 500)
    }

    pub fn max_consecutive_blank_lines(&self) -> i32 {
        self.max_consecutive_blank_lines_raw.clamp(0, 5)
    }
}
