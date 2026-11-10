//! WHERE-clause predicate building and SQL clause updating.
//!
//! Port of `ResultWhereActionBuilder.swift` + `ResultWherePredicateRenderer.swift`
//! + `SQLWhereClauseUpdater.swift`. Builds SQL WHERE predicates from selected
//!   cell values, renders them as SQL text, and inserts/removes them in a SQL
//!   statement.
//!
//! The predicate renderer and value classifier are pure and fully unit-tested.
//! The SQL clause updater is a pragmatic port that works on string slicing
//! (the Swift version used NSRange/NSString); it handles the common cases of
//! appending to an existing WHERE and inserting a new WHERE clause.

use sqlly_core::{CellValue, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;

use crate::sql_literal::{
    LikePlacement, like_clause_for, sql_identifier_for, sql_like_pattern_for, sql_literal_cell_for,
};

/// Operators that can appear in a WHERE action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WhereOperator {
    Equals,
    NotEquals,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    IsNull,
    IsNotNull,
    Like,
    NotLike,
    StartsWith,
    EndsWith,
    In,
    NotIn,
    Between,
}

impl WhereOperator {
    /// Whether the operator compares against a selected value; the NULL
    /// tests do not.
    pub fn takes_value(&self) -> bool {
        !matches!(self, WhereOperator::IsNull | WhereOperator::IsNotNull)
    }

    pub fn is_range_operator(&self) -> bool {
        matches!(
            self,
            WhereOperator::LessThan
                | WhereOperator::LessThanOrEqual
                | WhereOperator::GreaterThan
                | WhereOperator::GreaterThanOrEqual
                | WhereOperator::Between
        )
    }
}

/// The kind of value selected, used to decide which operators are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WhereValueKind {
    Null,
    Bool,
    Integer,
    Decimal,
    String,
    Date,
    Bytes,
}

/// A single selected value from the result grid, with its display text and
/// classified kind.
#[derive(Debug, Clone, PartialEq)]
pub struct WhereSelectedValue {
    pub cell_value: CellValue,
    pub display_text: String,
    pub kind: WhereValueKind,
    /// The grid showed a `<N bytes>` stand-in rather than the value, so
    /// `cell_value` is not the bytes themselves: the executed action must
    /// read them from the raw row, and where there is none the value cannot
    /// be compared at all. A genuine empty binary value is not a
    /// placeholder; it renders as `0x` / `X''` like any other.
    pub placeholder: bool,
}

/// The action mode: add a predicate, remove one, or copy it to the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhereActionMode {
    Add,
    #[allow(dead_code)]
    Remove,
    Copy,
    #[allow(dead_code)]
    MatchRow,
}

/// The logical join (AND/OR) used when appending to an existing WHERE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhereLogicalJoin {
    And,
    #[allow(dead_code)]
    Or,
}

/// A complete WHERE action built from selected cell values.
#[derive(Debug, Clone, PartialEq)]
pub struct WhereAction {
    pub title: String,
    pub column_name: String,
    pub column_index: usize,
    pub values: Vec<WhereSelectedValue>,
    pub operator: WhereOperator,
    pub mode: WhereActionMode,
    pub join: WhereLogicalJoin,
    pub predicate_preview: String,
    /// The underlying source column the result column came from, resolved
    /// from the executed statement (`p.Name` behind alias `PermissionName`).
    /// When present, predicates are written against this fully qualified
    /// expression; `None` keeps the legacy label-based rendering.
    pub source: Option<sqlly_intel::query::column_provenance::SourceColumn>,
}

impl WhereAction {
    /// Attach resolved column provenance and re-render the preview so the
    /// two never disagree. A `None` source leaves the action untouched.
    pub fn with_source(
        self,
        db: DatabaseType,
        source: Option<sqlly_intel::query::column_provenance::SourceColumn>,
    ) -> Option<WhereAction> {
        if source.is_none() {
            return Some(self);
        }
        let action = WhereAction { source, ..self };
        let preview = render_predicate_for(db, &action)?;
        Some(WhereAction {
            predicate_preview: preview,
            ..action
        })
    }
}

/// The identifier a predicate is written against: the resolved source column
/// fully qualified (alias raw when it needs no quoting, every other part in
/// the dialect's quoting), or the bare result label when no provenance is
/// known.
pub fn qualified_identifier_for(
    db: DatabaseType,
    source: Option<&sqlly_intel::query::column_provenance::SourceColumn>,
    fallback_label: &str,
) -> String {
    match source {
        Some(src) => {
            let mut parts: Vec<String> = src
                .qualifier
                .iter()
                .map(|part| quote_identifier_if_needed(db, part))
                .collect();
            parts.push(sql_identifier_for(db, &src.column));
            parts.join(".")
        }
        None => sql_identifier_for(db, fallback_label),
    }
}

/// Quotes a qualifier part only when it is not a plain identifier, so an
/// alias renders as `p.[Name]` rather than `[p].[Name]`.
fn quote_identifier_if_needed(db: DatabaseType, part: &str) -> String {
    let plain = !part.is_empty()
        && part
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '@' | '#'));
    if plain {
        part.to_string()
    } else {
        sql_identifier_for(db, part)
    }
}

/// Classifies a cell value + column data type into a [`WhereValueKind`].
///
/// The engine decides what a type name means where the names collide:
/// `timestamp` is a date-time on PostgreSQL and MySQL but SQL Server's
/// binary rowversion, which must not get the date or text operators even
/// when the grid shows it as hex text (a `binary=` directive, or the
/// menu's text-only path).
pub fn classify_value(
    db: DatabaseType,
    value: &CellValue,
    column: Option<&ResultColumn>,
) -> WhereValueKind {
    classify_value_for_type(Some(db), value, column.map(|c| c.data_type.as_str()))
}

/// [`classify_value`] with only the column's type name: the menu-building
/// path has the engine and a snapshot of type names, but no `ResultColumn`.
/// Only text is ambiguous: a number or a byte string classifies from its
/// value alone, and a text cell goes by the column's type where that type
/// says the text is not what it looks like (a date, a GUID, bytes shown as
/// hex), otherwise by what the text parses as.
pub fn classify_value_for_type(
    db: Option<DatabaseType>,
    value: &CellValue,
    data_type: Option<&str>,
) -> WhereValueKind {
    match (value, data_type) {
        (CellValue::String(_), Some(data_type)) => match kind_for_type_name(db, data_type) {
            Some(
                kind @ (WhereValueKind::Bytes | WhereValueKind::Date | WhereValueKind::String),
            ) => kind,
            _ => classify_cell(value),
        },
        _ => classify_cell(value),
    }
}

/// The kind a column's values filter as, from its type name, shared by the
/// menu (which has the type name) and the builder (which has the column),
/// so the two can never disagree about a column. One table serves the grid
/// and the row editor too: [`crate::result_cell_format::type_family`],
/// which also settles `timestamp` (SQL Server's binary rowversion there, a
/// date-time elsewhere; with no engine known, a date-time). `None` is a
/// type the table does not know, which a text value then decides.
pub fn kind_for_type_name(db: Option<DatabaseType>, data_type: &str) -> Option<WhereValueKind> {
    use crate::result_cell_format::TypeFamily;
    Some(
        match crate::result_cell_format::type_family(db, data_type)? {
            TypeFamily::Bool => WhereValueKind::Bool,
            TypeFamily::Integer => WhereValueKind::Integer,
            TypeFamily::Decimal => WhereValueKind::Decimal,
            TypeFamily::Date => WhereValueKind::Date,
            TypeFamily::Uuid => WhereValueKind::String,
            TypeFamily::Binary => WhereValueKind::Bytes,
        },
    )
}

/// Classifies a cell from its value alone, with no column and no engine:
/// the menu-building path, which only has the clicked cell. The engine
/// matters only where a type name is ambiguous, and there is no type name
/// here, so this can never reach the dialect-sensitive branch of
/// [`classify_value`].
pub fn classify_cell(value: &CellValue) -> WhereValueKind {
    match value {
        CellValue::Null => WhereValueKind::Null,
        CellValue::Bool(_) => WhereValueKind::Bool,
        CellValue::I64(_) => WhereValueKind::Integer,
        CellValue::F64(_) => WhereValueKind::Decimal,
        CellValue::String(s) => {
            if s.trim().parse::<i64>().is_ok() {
                WhereValueKind::Integer
            } else if s.trim().parse::<f64>().is_ok() {
                WhereValueKind::Decimal
            } else {
                WhereValueKind::String
            }
        }
        CellValue::Bytes(_) => WhereValueKind::Bytes,
    }
}

/// Parse a hex string (`0x…` or bare, even length) into bytes, or `None`
/// when it is not clean hex. Pure, so it is unit-testable.
pub(crate) fn hex_string_to_bytes(text: &str) -> Option<Vec<u8>> {
    let hex = text.trim();
    let hex = hex
        .strip_prefix("0x")
        .or_else(|| hex.strip_prefix("0X"))
        .unwrap_or(hex);
    if hex.is_empty() || !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

/// The bytes a `0x…` hex literal spells, when the text is one: a SQL Server
/// rowversion shown as hex (a `binary=` directive, or a grid with no raw
/// row to read) still has to filter as the bytes it is. Bare hex is not a
/// literal here: `deadbeef` in a text column is a word.
pub fn hex_literal_bytes(text: &str) -> Option<Vec<u8>> {
    let trimmed = text.trim();
    if !(trimmed.starts_with("0x") || trimmed.starts_with("0X")) {
        return None;
    }
    // A bare `0x` is the empty value written as hex, not a word.
    if trimmed.len() == 2 {
        return Some(Vec::new());
    }
    hex_string_to_bytes(trimmed)
}

/// Classifies a column from its data type alone (for menu availability
/// before a value is selected), from the same table as [`classify_value`];
/// a type the table does not know is text.
#[cfg(test)]
pub fn classify_column(db: DatabaseType, column: &ResultColumn) -> WhereValueKind {
    classify_column_for(Some(db), column)
}

/// [`classify_column`] with the engine as the host knows it: `None` reads
/// the type name engine-independently, the same way the menu did.
pub fn classify_column_for(db: Option<DatabaseType>, column: &ResultColumn) -> WhereValueKind {
    kind_for_type_name(db, &column.data_type).unwrap_or(WhereValueKind::String)
}

/// Returns the SQL literal for a single selected value, or `None` if it
/// cannot be rendered (e.g. Null for a non-IsNull operator).
pub fn single_sql_literal_for(db: DatabaseType, value: &WhereSelectedValue) -> Option<String> {
    match value.kind {
        WhereValueKind::Null => None,
        // A grid placeholder (`<16 bytes>`) classified as binary by its
        // column type is not the value and never renders. Hex text is the
        // value, spelled out. Any other text under a binary column is what
        // the engine returned for it (a SQLite BLOB holding text, a MySQL
        // binary collation), so it compares as that text rather than as
        // nothing.
        WhereValueKind::Bytes if value.placeholder => None,
        WhereValueKind::Bytes => match &value.cell_value {
            CellValue::Bytes(bytes) => Some(db.bytes_literal(bytes)),
            CellValue::String(text) => match hex_literal_bytes(text) {
                Some(bytes) => Some(db.bytes_literal(&bytes)),
                None => text_under_bytes_literal(db, text),
            },
            _ => None,
        },
        _ => sql_literal_cell_for(db, &value.cell_value),
    }
}

/// The literal for text the engine returned under a binary column. SQLite,
/// MySQL and PostgreSQL compare a binary column to a text literal as that
/// text (PostgreSQL reads an untyped literal as bytea input), so the text
/// is the value. SQL Server refuses the implicit nvarchar-to-varbinary
/// conversion and Oracle reads a text literal against RAW as hex, and
/// guessing what the text stands for (its own UTF-8 bytes? a base64
/// rendering the raw row should have answered?) would only produce a
/// predicate that runs and matches nothing: there the value is refused,
/// and the menu's availability gate refuses it the same way when it knows
/// the bytes cannot be read.
fn text_under_bytes_literal(db: DatabaseType, text: &str) -> Option<String> {
    if !text_compares_under_bytes(db) {
        return None;
    }
    sql_literal_cell_for(db, &CellValue::String(text.to_string()))
}

/// Whether the engine compares a binary column to a text literal as that
/// text (see [`text_under_bytes_literal`]): the renderer's rule, which the
/// availability gate asks too so the two never disagree.
fn text_compares_under_bytes(db: DatabaseType) -> bool {
    use sqlly_lang::sqlmodel::Dialect;
    match db.family() {
        Dialect::SqlServer | Dialect::Oracle => false,
        Dialect::Postgres | Dialect::MySql | Dialect::Sqlite => true,
    }
}

/// Renders a [`WhereAction`] as a T-SQL predicate string (tests and legacy
/// callers; prefer [`render_predicate_for`]).
#[cfg(test)]
pub fn render_predicate(action: &WhereAction) -> Option<String> {
    render_predicate_for(DatabaseType::SqlServer, action)
}

/// Renders a [`WhereAction`] as a SQL predicate in the engine's dialect:
/// identifier quoting, string/boolean literals, and LIKE escaping all follow
/// the engine the results came from.
pub fn render_predicate_for(db: DatabaseType, action: &WhereAction) -> Option<String> {
    let identifier = qualified_identifier_for(db, action.source.as_ref(), &action.column_name);
    match action.operator {
        WhereOperator::Equals => {
            let v = action.values.first()?;
            let lit = single_sql_literal_for(db, v)?;
            Some(format!("{} = {}", identifier, lit))
        }
        WhereOperator::NotEquals => {
            let v = action.values.first()?;
            let lit = single_sql_literal_for(db, v)?;
            Some(format!("{} <> {}", identifier, lit))
        }
        WhereOperator::LessThan => {
            let v = action.values.first()?;
            let lit = single_sql_literal_for(db, v)?;
            Some(format!("{} < {}", identifier, lit))
        }
        WhereOperator::LessThanOrEqual => {
            let v = action.values.first()?;
            let lit = single_sql_literal_for(db, v)?;
            Some(format!("{} <= {}", identifier, lit))
        }
        WhereOperator::GreaterThan => {
            let v = action.values.first()?;
            let lit = single_sql_literal_for(db, v)?;
            Some(format!("{} > {}", identifier, lit))
        }
        WhereOperator::GreaterThanOrEqual => {
            let v = action.values.first()?;
            let lit = single_sql_literal_for(db, v)?;
            Some(format!("{} >= {}", identifier, lit))
        }
        WhereOperator::IsNull => Some(format!("{} IS NULL", identifier)),
        WhereOperator::IsNotNull => Some(format!("{} IS NOT NULL", identifier)),
        WhereOperator::Like => {
            like_predicate(db, &identifier, action, LikePlacement::Contains, false)
        }
        WhereOperator::NotLike => {
            like_predicate(db, &identifier, action, LikePlacement::Contains, true)
        }
        WhereOperator::StartsWith => {
            like_predicate(db, &identifier, action, LikePlacement::StartsWith, false)
        }
        WhereOperator::EndsWith => {
            like_predicate(db, &identifier, action, LikePlacement::EndsWith, false)
        }
        WhereOperator::In => in_predicate(db, &identifier, action, false),
        WhereOperator::NotIn => in_predicate(db, &identifier, action, true),
        WhereOperator::Between => between_predicate(db, &identifier, action),
    }
}

fn like_predicate(
    db: DatabaseType,
    identifier: &str,
    action: &WhereAction,
    placement: LikePlacement,
    negated: bool,
) -> Option<String> {
    let v = action.values.first()?;
    // A rowversion shown as hex text is still bytes: a pattern over the
    // text would be rejected by the engine, so it does not render. SQLite
    // is the exception: it types dynamically, so a BLOB can sit under any
    // column, and LIKE reads a BLOB as its text. There a pattern over the
    // bytes is the pattern over the text they spell.
    let display = match (&v.cell_value, v.kind) {
        (_, WhereValueKind::Bytes) if v.placeholder => return None,
        (CellValue::Bytes(bytes), WhereValueKind::Bytes)
            if db.family() == sqlly_lang::sqlmodel::Dialect::Sqlite =>
        {
            String::from_utf8(bytes.clone()).ok()?
        }
        (CellValue::String(s), WhereValueKind::Bytes)
            if db.family() == sqlly_lang::sqlmodel::Dialect::Sqlite
                && hex_literal_bytes(s).is_none() =>
        {
            s.clone()
        }
        (_, WhereValueKind::Bytes) => return None,
        (CellValue::String(s), _) => s.clone(),
        _ => return None,
    };
    let pattern = sql_like_pattern_for(db, &display, placement);
    let kw = if negated { "NOT LIKE" } else { "LIKE" };
    Some(format!(
        "{} {} {}",
        identifier,
        kw,
        like_clause_for(db, &pattern)
    ))
}

fn in_predicate(
    db: DatabaseType,
    identifier: &str,
    action: &WhereAction,
    negated: bool,
) -> Option<String> {
    if action.values.is_empty() {
        return None;
    }
    let literals: Vec<String> = action
        .values
        .iter()
        .filter_map(|v| single_sql_literal_for(db, v))
        .collect();
    if literals.is_empty() {
        return None;
    }
    let kw = if negated { "NOT IN" } else { "IN" };
    Some(format!("{} {} ({})", identifier, kw, literals.join(", ")))
}

fn between_predicate(db: DatabaseType, identifier: &str, action: &WhereAction) -> Option<String> {
    if action.values.len() != 2 {
        return None;
    }
    let lo = single_sql_literal_for(db, &action.values[0])?;
    let hi = single_sql_literal_for(db, &action.values[1])?;
    Some(format!("{} BETWEEN {} AND {}", identifier, lo, hi))
}

/// Checks whether a candidate operator is available for a set of selected
/// values and a value kind. Returns `(is_available, reason)`.
///
/// `raw_rows_available` says whether the executed action can read the raw
/// bytes behind a binary cell (the host's rows; false on a pivot grid). It
/// settles binary text the menu cannot read as bytes itself (a
/// `binary=base64` directive): with raw rows the action compares the real
/// bytes, so the operator is offered; without them the text is all there
/// is, and only hex text spells bytes.
#[cfg(test)]
pub fn operator_availability(
    operator: WhereOperator,
    values: &[WhereSelectedValue],
    kind: WhereValueKind,
    raw_rows_available: bool,
) -> (bool, String) {
    operator_availability_for(
        DatabaseType::SqlServer,
        operator,
        values,
        kind,
        raw_rows_available,
    )
}

/// [`operator_availability`] in the engine's dialect: with no raw rows,
/// text under a binary column is a value exactly where the renderer
/// compares it as text (PostgreSQL, MySQL, SQLite), so the menu offers
/// what the click builds and no more.
pub fn operator_availability_for(
    db: DatabaseType,
    operator: WhereOperator,
    values: &[WhereSelectedValue],
    kind: WhereValueKind,
    raw_rows_available: bool,
) -> (bool, String) {
    // Binary values (a BINARY(16) UUID key, a hash) compare by equality;
    // text patterns and ranges over bytes mean nothing to the user.
    if kind == WhereValueKind::Bytes
        && !matches!(
            operator,
            WhereOperator::Equals
                | WhereOperator::NotEquals
                | WhereOperator::In
                | WhereOperator::NotIn
                | WhereOperator::IsNull
                | WhereOperator::IsNotNull
        )
    {
        return (
            false,
            "Binary values support only =, <>, IN, and NULL tests".to_string(),
        );
    }
    if !operator.takes_value() {
        return (true, String::new());
    }
    // A binary column whose bytes are nowhere to be read has nothing to
    // compare with; offering = on it would be a click that does nothing.
    // With no raw rows behind the grid, the only bytes there are come from
    // the cell itself: real bytes (empty ones included) or a hex literal
    // that spells them. A `<16 bytes>` placeholder or base64 text is not
    // them. A NULL among several selected values is skipped by the IN
    // rendering, not a missing value.
    if kind == WhereValueKind::Bytes
        && !raw_rows_available
        && values.iter().any(|v| {
            v.placeholder
                || match &v.cell_value {
                    CellValue::Bytes(_) | CellValue::Null => false,
                    CellValue::String(text) => {
                        hex_literal_bytes(text).is_none() && !text_compares_under_bytes(db)
                    }
                    _ => true,
                }
        })
    {
        return (
            false,
            "The value's bytes aren't available here; only the NULL tests apply".to_string(),
        );
    }
    if values.is_empty() {
        return (false, "No selected values".to_string());
    }
    if operator == WhereOperator::Between {
        return if values.len() == 2 {
            (true, String::new())
        } else {
            (false, "BETWEEN requires exactly two values".to_string())
        };
    }
    if operator.is_range_operator() && values.len() != 1 {
        return (
            false,
            "Range operators require exactly one value".to_string(),
        );
    }
    (true, String::new())
}

/// Builds a simple equals action for a single selected value (T-SQL preview;
/// tests and legacy callers — prefer [`build_equals_action_for`]).
#[cfg(test)]
pub fn build_equals_action(
    column_name: &str,
    column_index: usize,
    values: Vec<WhereSelectedValue>,
) -> Option<WhereAction> {
    build_equals_action_for(DatabaseType::SqlServer, column_name, column_index, values)
}

/// Builds a simple equals action for a single selected value (the most common
/// context-menu action: "WHERE column = value") with the engine's rendering.
pub fn build_equals_action_for(
    db: DatabaseType,
    column_name: &str,
    column_index: usize,
    values: Vec<WhereSelectedValue>,
) -> Option<WhereAction> {
    let first = values.first()?;
    let operator = if first.kind == WhereValueKind::Null {
        WhereOperator::IsNull
    } else {
        WhereOperator::Equals
    };
    let action = WhereAction {
        title: format!("WHERE {} = ...", column_name),
        column_name: column_name.to_string(),
        column_index,
        values,
        operator,
        mode: WhereActionMode::Add,
        join: WhereLogicalJoin::And,
        predicate_preview: String::new(),
        source: None,
    };
    let preview = render_predicate_for(db, &action)?;
    Some(WhereAction {
        title: format!("Add WHERE {}", column_name),
        predicate_preview: preview,
        ..action
    })
}

/// Builds an IN action for multiple selected values (T-SQL preview; tests
/// and legacy callers — prefer [`build_in_action_for`]).
#[cfg(test)]
pub fn build_in_action(
    column_name: &str,
    column_index: usize,
    values: Vec<WhereSelectedValue>,
) -> Option<WhereAction> {
    build_in_action_for(DatabaseType::SqlServer, column_name, column_index, values)
}

/// Builds an IN action for multiple selected values with the engine's
/// rendering.
pub fn build_in_action_for(
    db: DatabaseType,
    column_name: &str,
    column_index: usize,
    values: Vec<WhereSelectedValue>,
) -> Option<WhereAction> {
    if values.is_empty() {
        return None;
    }
    let action = WhereAction {
        title: format!("WHERE {} IN (...)", column_name),
        column_name: column_name.to_string(),
        column_index,
        values,
        operator: WhereOperator::In,
        mode: WhereActionMode::Add,
        join: WhereLogicalJoin::And,
        predicate_preview: String::new(),
        source: None,
    };
    let preview = render_predicate_for(db, &action)?;
    Some(WhereAction {
        title: format!("Add WHERE {} IN (...)", column_name),
        predicate_preview: preview,
        ..action
    })
}

/// Result of applying a WHERE action to a SQL statement.
#[derive(Debug, Clone, PartialEq)]
pub struct WhereUpdateResult {
    pub updated_sql: String,
    pub diagnostics: Vec<String>,
}

/// Applies a WHERE action to a SQL statement with T-SQL rendering (tests
/// and legacy callers; prefer [`apply_where_action_for`]).
#[cfg(test)]
pub fn apply_where_action(sql: &str, action: &WhereAction) -> WhereUpdateResult {
    apply_where_action_for(DatabaseType::SqlServer, sql, action)
}

/// Applies a WHERE action to a SQL statement: appends the predicate to an
/// existing WHERE clause, or inserts a new WHERE clause. The predicate
/// renders in the engine's dialect.
pub fn apply_where_action_for(
    db: DatabaseType,
    sql: &str,
    action: &WhereAction,
) -> WhereUpdateResult {
    if action.mode == WhereActionMode::Copy {
        return WhereUpdateResult {
            updated_sql: sql.to_string(),
            diagnostics: vec!["Copied predicate to pasteboard".to_string()],
        };
    }

    let predicate = match render_predicate_for(db, action) {
        Some(p) => p,
        None => {
            return WhereUpdateResult {
                updated_sql: sql.to_string(),
                diagnostics: vec![format!(
                    "Failed to render predicate for {:?}",
                    action.operator
                )],
            };
        }
    };

    let joiner = match action.join {
        WhereLogicalJoin::Or => "OR",
        WhereLogicalJoin::And => "AND",
    };

    // Find existing WHERE clause (case-insensitive, not inside strings/comments).
    let where_pos = find_where_keyword(sql);
    let insert_pos = find_predicate_insertion_point(sql);

    let updated = match where_pos {
        Some(pos) => {
            // Append to existing WHERE
            let insertion = format!(" {} {}", joiner, predicate);
            let mut result = String::with_capacity(sql.len() + insertion.len());
            result.push_str(&sql[..pos.end]);
            result.push_str(&insertion);
            result.push_str(&sql[pos.end..]);
            result
        }
        None => {
            // Insert new WHERE clause at the insertion point
            let needs_space = insert_pos > 0
                && !sql[..insert_pos].ends_with(' ')
                && !sql[..insert_pos].ends_with('\n');
            let mut result = String::with_capacity(sql.len() + predicate.len() + 10);
            result.push_str(&sql[..insert_pos]);
            if needs_space {
                result.push(' ');
            }
            result.push_str("WHERE ");
            result.push_str(&predicate);
            result.push_str(&sql[insert_pos..]);
            result
        }
    };

    WhereUpdateResult {
        updated_sql: updated,
        diagnostics: vec![format!("Appended {} predicate", joiner)],
    }
}

/// A found WHERE keyword position in the SQL text.
struct KeywordPosition {
    #[allow(dead_code)]
    start: usize,
    end: usize,
}

/// Byte width of the char starting at byte `i` (1 if `i` is past the end or —
/// defensively — not on a boundary). Used to advance the byte cursor one
/// character at a time without ever landing inside a multi-byte scalar, which
/// would panic the next time the loop slices `&sql[i..]`.
fn char_width_at(s: &str, i: usize) -> usize {
    s.get(i..)
        .and_then(|rest| rest.chars().next())
        .map_or(1, char::len_utf8)
}

/// Finds the WHERE keyword in the SQL text, skipping strings and comments.
/// Returns the byte position just after "WHERE".
fn find_where_keyword(sql: &str) -> Option<KeywordPosition> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    let mut in_bracket = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let remaining = &sql[i..];

        if in_line_comment {
            if remaining.starts_with('\n') {
                in_line_comment = false;
                i += 1;
            } else {
                i += char_width_at(sql, i);
            }
            continue;
        }
        if in_block_comment {
            if remaining.starts_with("*/") {
                in_block_comment = false;
                i += 2;
            } else {
                i += char_width_at(sql, i);
            }
            continue;
        }
        if in_string {
            if let Some(rest) = remaining.strip_prefix('\'') {
                if rest.starts_with('\'') {
                    i += 2; // escaped quote
                } else {
                    in_string = false;
                    i += 1;
                }
            } else {
                i += char_width_at(sql, i);
            }
            continue;
        }
        if in_bracket {
            if remaining.starts_with(']') {
                in_bracket = false;
                i += 1;
            } else {
                i += char_width_at(sql, i);
            }
            continue;
        }

        if remaining.starts_with("/*") {
            in_block_comment = true;
            i += 2;
        } else if remaining.starts_with("--") {
            in_line_comment = true;
            i += 2;
        } else if remaining.starts_with('\'') {
            in_string = true;
            i += 1;
        } else if remaining.starts_with('[') {
            in_bracket = true;
            i += 1;
        } else if remaining
            .get(..5)
            .is_some_and(|w| w.eq_ignore_ascii_case("WHERE"))
            && (remaining.len() == 5 || !remaining.as_bytes()[5].is_ascii_alphanumeric())
        {
            // Check that the char before is not alphanumeric (word boundary)
            if i == 0 || !bytes[i - 1].is_ascii_alphanumeric() {
                return Some(KeywordPosition {
                    start: i,
                    end: i + 5,
                });
            }
            i += 1;
        } else {
            i += char_width_at(sql, i);
        }
    }
    None
}

/// Finds the insertion point for a new WHERE clause: just before GROUP BY,
/// ORDER BY, HAVING, UNION, EXCEPT, INTERSECT, OPTION, or FOR — or at end of
/// statement.
fn find_predicate_insertion_point(sql: &str) -> usize {
    let keywords = [
        "GROUP BY",
        "ORDER BY",
        "HAVING",
        "UNION",
        "EXCEPT",
        "INTERSECT",
        "OPTION",
        "FOR ",
    ];
    // Search the original `sql` bytes with an ASCII-case-insensitive compare.
    // The keywords are all ASCII, so any match lands on a char boundary and the
    // offsets stay valid for slicing `sql`. (An earlier version searched a
    // `sql.to_lowercase()` copy and indexed the original with those offsets;
    // `to_lowercase` is not byte-length preserving — e.g. `İ` → `i̇` — so a
    // length-changing uppercase char before the keyword drifted the offset,
    // slicing mid-scalar or past the end and panicking.)
    let bytes = sql.as_bytes();
    let mut earliest: Option<usize> = None;
    for keyword in &keywords {
        let kw = keyword.as_bytes();
        if kw.len() > bytes.len() {
            continue;
        }
        let mut i = 0;
        while i + kw.len() <= bytes.len() {
            if bytes[i..i + kw.len()].eq_ignore_ascii_case(kw)
                // Check word boundary before.
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            {
                earliest = Some(match earliest {
                    Some(e) if e < i => e,
                    _ => i,
                });
                break;
            }
            i += 1;
        }
    }
    earliest.unwrap_or(sql.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{CellValue, ResultColumn};

    #[test]
    fn find_where_keyword_survives_multibyte_comments() {
        // The byte-walking scanner must advance a full char at a time so a
        // Spanish comment never leaves the cursor mid-scalar (past crash).
        let sql = "UPDATE t SET c = 1 -- Marcó salida\nWHERE id = 1";
        let pos = find_where_keyword(sql).expect("WHERE found after accented comment");
        assert_eq!(&sql[pos.start..pos.end], "WHERE");
    }

    #[test]
    fn find_where_keyword_multibyte_in_string_and_bracket() {
        // Accented text inside a string literal and a bracketed identifier.
        let sql = "UPDATE [tablá] SET c = 'niño' WHERE id = 1";
        let pos = find_where_keyword(sql).expect("WHERE found");
        assert_eq!(&sql[pos.start..pos.end], "WHERE");
        // A statement whose only non-ASCII sits in code position must not panic.
        assert!(find_where_keyword("SELECT café FROM t").is_none());
    }

    #[test]
    fn insertion_point_survives_length_changing_uppercase() {
        // Regression: the search ran over `sql.to_lowercase()` and indexed the
        // original `sql` with those offsets. `İ` (U+0130) lowercases to two
        // bytes, so a keyword after it drifted the offset — slicing mid-scalar
        // or past the end and panicking when a WHERE was inserted.
        let sql = "SELECT İ FROM t ORDER BY x";
        let pos = find_predicate_insertion_point(sql);
        assert!(sql.is_char_boundary(pos), "insertion point off a boundary");
        assert_eq!(&sql[pos..], "ORDER BY x");

        // The full apply path (what a right-click WHERE action runs) must not
        // panic and must place the clause before ORDER BY.
        let action = WhereAction {
            title: "t".into(),
            column_name: "x".into(),
            column_index: 0,
            values: vec![val(&CellValue::I64(1), "1", WhereValueKind::Integer)],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: "[x] = 1".into(),
            source: None,
        };
        let out = apply_where_action_for(DatabaseType::SqlServer, sql, &action);
        assert!(out.updated_sql.contains("WHERE"));
        assert!(
            out.updated_sql.find("WHERE") < out.updated_sql.find("ORDER BY"),
            "WHERE should precede ORDER BY: {}",
            out.updated_sql
        );
    }

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable: true,
        }
    }

    fn val(value: &CellValue, display: &str, kind: WhereValueKind) -> WhereSelectedValue {
        WhereSelectedValue {
            cell_value: value.clone(),
            display_text: display.to_string(),
            kind,
            placeholder: false,
        }
    }

    #[test]
    fn classify_integer_value() {
        assert_eq!(
            classify_value(
                DatabaseType::SqlServer,
                &CellValue::I64(42),
                Some(&col("c", "int"))
            ),
            WhereValueKind::Integer
        );
    }

    #[test]
    fn classify_string_value_as_integer_when_numeric() {
        assert_eq!(
            classify_value(
                DatabaseType::SqlServer,
                &CellValue::String("42".into()),
                Some(&col("c", "varchar"))
            ),
            WhereValueKind::Integer
        );
    }

    #[test]
    fn classify_string_value_as_string_when_text() {
        assert_eq!(
            classify_value(
                DatabaseType::SqlServer,
                &CellValue::String("hello".into()),
                Some(&col("c", "varchar"))
            ),
            WhereValueKind::String
        );
    }

    /// PostgreSQL and MySQL report date-time columns as `timestamp`; SQL
    /// Server's `timestamp` is the binary rowversion. The engine, not the
    /// cell's text, says which one a column is.
    #[test]
    fn timestamp_is_a_date_on_postgres_and_mysql() {
        for db in [DatabaseType::Postgres, DatabaseType::MySql] {
            assert_eq!(
                classify_value(
                    db,
                    &CellValue::String("2024-05-01 10:00:00".into()),
                    Some(&col("created_at", "timestamp"))
                ),
                WhereValueKind::Date,
                "{db:?}"
            );
            assert_eq!(
                classify_column(db, &col("created_at", "timestamp")),
                WhereValueKind::Date,
                "{db:?}"
            );
        }
        assert_eq!(
            classify_value(
                DatabaseType::Postgres,
                &CellValue::String("2024-05-01 10:00:00+00".into()),
                Some(&col("created_at", "timestamptz"))
            ),
            WhereValueKind::Date
        );
    }

    /// `rowversion` names SQL Server's binary row stamp and nothing else, so
    /// it is bytes even when the result claims another engine (a misreported
    /// family, a type snapshot keyed by the wrong dialect): never text, so
    /// never LIKE.
    #[test]
    fn rowversion_is_bytes_on_every_engine() {
        for db in [
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::Sqlite,
        ] {
            assert_eq!(
                classify_column(db, &col("rv", "rowversion")),
                WhereValueKind::Bytes,
                "{db:?}"
            );
            assert_eq!(
                classify_value(
                    db,
                    &CellValue::String("0x00000000000007D1".into()),
                    Some(&col("rv", "rowversion"))
                ),
                WhereValueKind::Bytes,
                "{db:?}"
            );
        }
    }

    /// A SQL Server rowversion is bytes whether it arrives as bytes or as
    /// hex text, and whether the column is reported as `timestamp` or
    /// `rowversion`.
    #[test]
    fn timestamp_and_rowversion_are_bytes_on_sql_server() {
        for data_type in ["timestamp", "rowversion"] {
            assert_eq!(
                classify_value(
                    DatabaseType::SqlServer,
                    &CellValue::Bytes(vec![0; 8]),
                    Some(&col("rv", data_type))
                ),
                WhereValueKind::Bytes,
                "{data_type}"
            );
            assert_eq!(
                classify_value(
                    DatabaseType::SqlServer,
                    &CellValue::String("0x00000000000007D1".into()),
                    Some(&col("rv", data_type))
                ),
                WhereValueKind::Bytes,
                "{data_type}"
            );
            assert_eq!(
                classify_column(DatabaseType::SqlServer, &col("rv", data_type)),
                WhereValueKind::Bytes,
                "{data_type}"
            );
        }
    }

    /// So the menu offers only =, <>, IN and the NULL tests on a rowversion,
    /// never LIKE or a range.
    #[test]
    fn rowversion_hex_text_gets_only_the_binary_operators() {
        let cell = CellValue::String("0x00000000000007D1".into());
        let kind = classify_value(
            DatabaseType::SqlServer,
            &cell,
            Some(&col("rv", "rowversion")),
        );
        let values = vec![val(&cell, "0x00000000000007D1", kind)];
        for op in [
            WhereOperator::Like,
            WhereOperator::StartsWith,
            WhereOperator::GreaterThan,
            WhereOperator::Between,
        ] {
            assert!(!operator_availability(op, &values, kind, false).0, "{op:?}");
        }
        for op in [
            WhereOperator::Equals,
            WhereOperator::NotEquals,
            WhereOperator::IsNull,
            WhereOperator::IsNotNull,
        ] {
            assert!(operator_availability(op, &values, kind, false).0, "{op:?}");
        }
    }

    /// LIKE over the hex text of a rowversion is not a text search: the
    /// engine rejects it, so the renderer refuses it the same way the menu
    /// does.
    #[test]
    fn like_does_not_render_on_bytes_shown_as_hex_text() {
        let cell = CellValue::String("0x00000000000007D1".into());
        for operator in [
            WhereOperator::Like,
            WhereOperator::NotLike,
            WhereOperator::StartsWith,
            WhereOperator::EndsWith,
        ] {
            let action = WhereAction {
                title: "test".into(),
                column_name: "rv".into(),
                column_index: 0,
                values: vec![val(&cell, "0x00000000000007D1", WhereValueKind::Bytes)],
                operator,
                mode: WhereActionMode::Add,
                join: WhereLogicalJoin::And,
                predicate_preview: String::new(),
                source: None,
            };
            assert_eq!(
                render_predicate_for(DatabaseType::SqlServer, &action),
                None,
                "{operator:?}"
            );
        }
    }

    /// Two cells selected in a varbinary column, one of them NULL: IN
    /// skips the NULL, as it always has, instead of refusing the whole
    /// selection as "bytes not available".
    #[test]
    fn in_on_bytes_with_a_null_among_them_skips_the_null() {
        let values = vec![
            val(
                &CellValue::Bytes(vec![0xAB, 0xCD]),
                "<2 bytes>",
                WhereValueKind::Bytes,
            ),
            val(&CellValue::Null, "NULL", WhereValueKind::Null),
        ];
        assert!(operator_availability(WhereOperator::In, &values, WhereValueKind::Bytes, false).0);
        let action = build_in_action_for(DatabaseType::SqlServer, "key", 0, values)
            .expect("IN builds past the NULL");
        assert_eq!(action.predicate_preview, "[key] IN (0xABCD)");
    }

    /// A rowversion the grid shows as hex text, with no raw row to read
    /// (a pivot grid, a `binary=` directive): the filter is the bytes the
    /// text spells, not a string, and not a dead click.
    #[test]
    fn rowversion_hex_text_filters_as_the_bytes_it_spells() {
        let cell = CellValue::String("0x00000000000007D1".into());
        let kind = classify_value(
            DatabaseType::SqlServer,
            &cell,
            Some(&col("rv", "rowversion")),
        );
        let action = WhereAction {
            title: "test".into(),
            column_name: "rv".into(),
            column_index: 0,
            values: vec![val(&cell, "0x00000000000007D1", kind)],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        assert_eq!(
            render_predicate_for(DatabaseType::SqlServer, &action).as_deref(),
            Some("[rv] = 0x00000000000007D1")
        );

        // Text that is not a hex literal has no bytes to compare with, so
        // only the NULL tests stay on the menu.
        let odd = CellValue::String("<16 bytes, cut>".into());
        let values = vec![val(&odd, "<16 bytes, cut>", WhereValueKind::Bytes)];
        assert!(
            !operator_availability(WhereOperator::Equals, &values, WhereValueKind::Bytes, false).0
        );
        assert!(
            operator_availability(WhereOperator::IsNull, &values, WhereValueKind::Bytes, false).0
        );
    }

    /// An empty binary value shown as hex (`0x`) is still a value: it
    /// renders as the engine's empty bytes literal everywhere.
    #[test]
    fn bare_hex_prefix_is_the_empty_value() {
        let value = WhereSelectedValue {
            cell_value: CellValue::String("0x".into()),
            display_text: "0x".into(),
            kind: WhereValueKind::Bytes,
            placeholder: false,
        };
        assert_eq!(
            single_sql_literal_for(DatabaseType::SqlServer, &value).as_deref(),
            Some("0x")
        );
        assert_eq!(
            single_sql_literal_for(DatabaseType::Sqlite, &value).as_deref(),
            Some("X''")
        );
        assert_eq!(
            single_sql_literal_for(DatabaseType::MySql, &value).as_deref(),
            Some("0x")
        );
    }

    /// With no raw rows (a pivot grid), text under a binary column is a
    /// value exactly where the renderer compares it as text, so the gate
    /// and the renderer give one answer.
    #[test]
    fn availability_follows_the_renderer_for_text_under_bytes() {
        let values = [WhereSelectedValue {
            cell_value: CellValue::String("abc".into()),
            display_text: "abc".into(),
            kind: WhereValueKind::Bytes,
            placeholder: false,
        }];
        for db in [
            DatabaseType::MySql,
            DatabaseType::Postgres,
            DatabaseType::Sqlite,
        ] {
            assert!(
                operator_availability_for(
                    db,
                    WhereOperator::Equals,
                    &values,
                    WhereValueKind::Bytes,
                    false
                )
                .0,
                "{db:?}"
            );
            assert!(single_sql_literal_for(db, &values[0]).is_some(), "{db:?}");
        }
        for db in [DatabaseType::SqlServer, DatabaseType::Oracle] {
            assert!(
                !operator_availability_for(
                    db,
                    WhereOperator::Equals,
                    &values,
                    WhereValueKind::Bytes,
                    false
                )
                .0,
                "{db:?}"
            );
            assert!(single_sql_literal_for(db, &values[0]).is_none(), "{db:?}");
        }
    }

    #[test]
    fn hex_literal_bytes_reads_only_well_formed_literals() {
        assert_eq!(hex_literal_bytes("0x0A"), Some(vec![0x0A]));
        assert_eq!(
            hex_literal_bytes(" 0XdeadBEEF "),
            Some(vec![0xDE, 0xAD, 0xBE, 0xEF])
        );
        assert_eq!(hex_literal_bytes("0x"), Some(vec![]));
        assert_eq!(hex_literal_bytes("0xABC"), None);
        assert_eq!(hex_literal_bytes("0xZZ"), None);
        assert_eq!(hex_literal_bytes("deadbeef"), None);
    }

    #[test]
    fn classify_null_value() {
        assert_eq!(
            classify_value(
                DatabaseType::SqlServer,
                &CellValue::Null,
                Some(&col("c", "int"))
            ),
            WhereValueKind::Null
        );
    }

    #[test]
    fn classify_column_int() {
        assert_eq!(
            classify_column(DatabaseType::SqlServer, &col("c", "int")),
            WhereValueKind::Integer
        );
    }

    #[test]
    fn classify_column_datetime() {
        assert_eq!(
            classify_column(DatabaseType::SqlServer, &col("c", "datetime")),
            WhereValueKind::Date
        );
    }

    #[test]
    fn classify_column_binary() {
        assert_eq!(
            classify_column(DatabaseType::SqlServer, &col("c", "varbinary(max)")),
            WhereValueKind::Bytes
        );
    }

    /// Oracle's RAW is bytes for the menu (type name only) and the builder
    /// (the column), from the one table; and `LONG RAW` keeps both words,
    /// which the grid's base-name split would cut to `long`.
    #[test]
    fn oracle_raw_is_bytes_for_menu_and_builder() {
        let hex = CellValue::String("0x0A0B".into());
        for data_type in ["RAW", "RAW(16)", "LONG RAW"] {
            assert_eq!(
                classify_value_for_type(Some(DatabaseType::Oracle), &hex, Some(data_type)),
                WhereValueKind::Bytes,
                "{data_type} for the menu"
            );
            assert_eq!(
                classify_column(DatabaseType::Oracle, &col("c", data_type)),
                WhereValueKind::Bytes,
                "{data_type} for the builder"
            );
        }
    }

    /// The WHERE menu, the grid and the row editor classify a column from
    /// the one type table, so a binary column is binary to all three and a
    /// SQL Server `timestamp` is bytes to all three where PostgreSQL's is a
    /// date to all three.
    #[test]
    fn where_grid_and_editor_agree_on_binary_columns() {
        use crate::preferences::ResultKind;
        use crate::result_cell_format::{TypeFamily, infer_result_kind_for, type_family};
        let engines = [
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::Oracle,
            DatabaseType::Sqlite,
        ];
        for db in engines {
            for data_type in [
                "rowversion",
                "timestamp",
                "raw",
                "long raw",
                "blob",
                "varbinary(16)",
            ] {
                let family = type_family(Some(db), data_type);
                let column = col("c", data_type);
                let where_bytes = classify_column(db, &column) == WhereValueKind::Bytes;
                let editor_bytes = crate::cell_actions::is_binary_type_for(Some(db), data_type);
                let grid = infer_result_kind_for(Some(db), Some(&column));
                assert_eq!(
                    where_bytes,
                    family == Some(TypeFamily::Binary),
                    "{db:?} {data_type}: menu vs table"
                );
                assert_eq!(
                    editor_bytes, where_bytes,
                    "{db:?} {data_type}: editor vs menu"
                );
                if where_bytes {
                    assert_eq!(grid, ResultKind::Text, "{db:?} {data_type}: grid");
                } else {
                    assert_eq!(grid, ResultKind::Date, "{db:?} {data_type}: grid");
                    assert_eq!(data_type, "timestamp");
                }
            }
        }
        assert!(crate::cell_actions::is_binary_type_for(
            Some(DatabaseType::SqlServer),
            "timestamp"
        ));
        assert!(!crate::cell_actions::is_binary_type_for(
            Some(DatabaseType::Postgres),
            "timestamp"
        ));
    }

    /// A size or precision group goes, whatever follows it stays: the
    /// qualified spellings PostgreSQL reports keep their qualifier.
    #[test]
    fn where_type_name_drops_only_the_size_group() {
        use crate::result_cell_format::type_name_key;
        assert_eq!(
            type_name_key("timestamp(6) with time zone"),
            "timestamp with time zone"
        );
        assert_eq!(
            type_name_key("time(3) with time zone"),
            "time with time zone"
        );
        assert_eq!(type_name_key("numeric(10, 2)"), "numeric");
        assert_eq!(type_name_key("RAW(16)"), "raw");
        assert_eq!(type_name_key("  Long   Raw "), "long raw");
        assert_eq!(type_name_key("Nullable(String)"), "string");
        assert_eq!(
            kind_for_type_name(Some(DatabaseType::Postgres), "timestamp(6) with time zone"),
            Some(WhereValueKind::Date)
        );
    }

    /// A real empty binary value renders (`0x`) and is offered without raw
    /// rows; only the menu's `<N bytes>` stand-in is refused there.
    #[test]
    fn empty_bytes_are_a_value_but_a_placeholder_is_not() {
        let empty = [val(&CellValue::Bytes(vec![]), "0x", WhereValueKind::Bytes)];
        let (avail, _) =
            operator_availability(WhereOperator::Equals, &empty, WhereValueKind::Bytes, false);
        assert!(avail, "an empty value has a literal");
        assert_eq!(
            single_sql_literal_for(DatabaseType::SqlServer, &empty[0]).as_deref(),
            Some("0x")
        );
        let mut stand_in = val(
            &CellValue::Bytes(vec![]),
            "<16 bytes>",
            WhereValueKind::Bytes,
        );
        stand_in.placeholder = true;
        let (avail, reason) = operator_availability(
            WhereOperator::Equals,
            std::slice::from_ref(&stand_in),
            WhereValueKind::Bytes,
            false,
        );
        assert!(!avail, "{reason}");
        let (avail, _) = operator_availability(
            WhereOperator::Equals,
            std::slice::from_ref(&stand_in),
            WhereValueKind::Bytes,
            true,
        );
        assert!(avail, "with raw rows the executed action reads the bytes");
    }

    /// Multi-word type names classify as the whole name, on both paths.
    #[test]
    fn multi_word_type_names_classify_whole() {
        let pg = DatabaseType::Postgres;
        assert_eq!(
            classify_column(pg, &col("c", "DOUBLE PRECISION")),
            WhereValueKind::Decimal
        );
        assert_eq!(
            classify_column(pg, &col("c", "TIMESTAMP WITH TIME ZONE")),
            WhereValueKind::Date
        );
        let text = CellValue::String("2024-05-01 10:00:00+00".into());
        assert_eq!(
            classify_value_for_type(Some(pg), &text, Some("timestamp without time zone")),
            WhereValueKind::Date
        );
        assert_eq!(
            classify_value_for_type(
                Some(pg),
                &CellValue::String("1.5".into()),
                Some("double precision")
            ),
            WhereValueKind::Decimal
        );
        // The table stays engine-aware: `timestamp` is bytes only on SQL Server.
        assert_eq!(
            kind_for_type_name(Some(pg), "timestamp"),
            Some(WhereValueKind::Date)
        );
        assert_eq!(
            kind_for_type_name(Some(DatabaseType::SqlServer), "timestamp"),
            Some(WhereValueKind::Bytes)
        );
        assert_eq!(kind_for_type_name(Some(pg), "hstore"), None);
    }

    #[test]
    fn render_equals_predicate() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "CustomerID".into(),
            column_index: 0,
            values: vec![val(&CellValue::I64(42), "42", WhereValueKind::Integer)],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        assert_eq!(
            render_predicate(&action),
            Some("[CustomerID] = 42".to_string())
        );
    }

    #[test]
    fn render_string_equals_predicate() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Name".into(),
            column_index: 0,
            values: vec![val(
                &CellValue::String("O'Brien".into()),
                "O'Brien",
                WhereValueKind::String,
            )],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        assert_eq!(
            render_predicate(&action),
            Some("[Name] = N'O''Brien'".to_string())
        );
    }

    /// With resolved provenance the predicate targets the alias-qualified
    /// source column, never the SELECT-list alias — `p.[Name]`, alias raw,
    /// column in the dialect's quoting.
    #[test]
    fn render_qualified_predicate_uses_the_source_column() {
        use sqlly_intel::query::column_provenance::SourceColumn;
        let action = build_equals_action(
            "PermissionName",
            0,
            vec![val(
                &CellValue::String("CREATE".into()),
                "CREATE",
                WhereValueKind::String,
            )],
        )
        .unwrap()
        .with_source(
            DatabaseType::SqlServer,
            Some(SourceColumn {
                qualifier: vec!["p".into()],
                column: "Name".into(),
            }),
        )
        .unwrap();
        assert_eq!(action.predicate_preview, "p.[Name] = N'CREATE'");

        // Schema-qualified chain when the query had no alias, and dialect
        // quoting per engine.
        let pg = WhereAction {
            source: Some(SourceColumn {
                qualifier: vec!["Shared".into(), "Permission".into()],
                column: "Name".into(),
            }),
            ..action.clone()
        };
        let rendered = render_predicate_for(DatabaseType::Postgres, &pg).unwrap();
        assert!(
            rendered.starts_with("Shared.Permission."),
            "plain qualifier parts stay raw: {rendered}"
        );
        assert!(
            rendered.contains("\"Name\""),
            "PG quotes the column part: {rendered}"
        );

        // A qualifier that is not a plain identifier gets quoted.
        let odd = WhereAction {
            source: Some(SourceColumn {
                qualifier: vec!["my alias".into()],
                column: "Name".into(),
            }),
            ..action
        };
        assert_eq!(
            render_predicate_for(DatabaseType::SqlServer, &odd),
            Some("[my alias].[Name] = N'CREATE'".to_string())
        );
    }

    #[test]
    fn render_is_null_predicate() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Email".into(),
            column_index: 0,
            values: vec![val(&CellValue::Null, "NULL", WhereValueKind::Null)],
            operator: WhereOperator::IsNull,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        assert_eq!(
            render_predicate(&action),
            Some("[Email] IS NULL".to_string())
        );
    }

    #[test]
    fn render_is_not_null_predicate() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Email".into(),
            column_index: 0,
            values: vec![],
            operator: WhereOperator::IsNotNull,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        assert_eq!(
            render_predicate(&action),
            Some("[Email] IS NOT NULL".to_string())
        );
    }

    #[test]
    fn render_in_predicate() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Status".into(),
            column_index: 0,
            values: vec![
                val(
                    &CellValue::String("Active".into()),
                    "Active",
                    WhereValueKind::String,
                ),
                val(
                    &CellValue::String("Pending".into()),
                    "Pending",
                    WhereValueKind::String,
                ),
            ],
            operator: WhereOperator::In,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        assert_eq!(
            render_predicate(&action),
            Some("[Status] IN (N'Active', N'Pending')".to_string())
        );
    }

    #[test]
    fn render_between_predicate() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Price".into(),
            column_index: 0,
            values: vec![
                val(&CellValue::I64(10), "10", WhereValueKind::Integer),
                val(&CellValue::I64(100), "100", WhereValueKind::Integer),
            ],
            operator: WhereOperator::Between,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        assert_eq!(
            render_predicate(&action),
            Some("[Price] BETWEEN 10 AND 100".to_string())
        );
    }

    #[test]
    fn render_between_requires_two_values() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Price".into(),
            column_index: 0,
            values: vec![val(&CellValue::I64(10), "10", WhereValueKind::Integer)],
            operator: WhereOperator::Between,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        assert_eq!(render_predicate(&action), None);
    }

    #[test]
    fn render_like_contains_predicate() {
        // LIKE renders a contains-style `%value%` pattern.
        let action = WhereAction {
            title: "test".into(),
            column_name: "Notes".into(),
            column_index: 0,
            values: vec![val(
                &CellValue::String("urgent".into()),
                "urgent",
                WhereValueKind::String,
            )],
            operator: WhereOperator::Like,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let result = render_predicate(&action).unwrap();
        assert!(result.contains("[Notes] LIKE"));
        assert!(result.contains("%urgent%"));
    }

    #[test]
    fn render_not_like_predicate() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Notes".into(),
            column_index: 0,
            values: vec![val(
                &CellValue::String("spam".into()),
                "spam",
                WhereValueKind::String,
            )],
            operator: WhereOperator::NotLike,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let result = render_predicate(&action).unwrap();
        assert!(result.contains("[Notes] NOT LIKE"), "{result}");
        assert!(result.contains("%spam%"), "{result}");
    }

    #[test]
    fn render_comparison_predicates() {
        for (op, symbol) in [
            (WhereOperator::LessThan, "<"),
            (WhereOperator::LessThanOrEqual, "<="),
            (WhereOperator::GreaterThan, ">"),
            (WhereOperator::GreaterThanOrEqual, ">="),
        ] {
            let action = WhereAction {
                title: "test".into(),
                column_name: "Price".into(),
                column_index: 0,
                values: vec![val(&CellValue::I64(50), "50", WhereValueKind::Integer)],
                operator: op,
                mode: WhereActionMode::Add,
                join: WhereLogicalJoin::And,
                predicate_preview: String::new(),
                source: None,
            };
            assert_eq!(
                render_predicate(&action),
                Some(format!("[Price] {symbol} 50")),
                "operator {op:?}"
            );
        }
    }

    #[test]
    fn render_starts_with_predicate() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Name".into(),
            column_index: 0,
            values: vec![val(
                &CellValue::String("John".into()),
                "John",
                WhereValueKind::String,
            )],
            operator: WhereOperator::StartsWith,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let result = render_predicate(&action).unwrap();
        assert!(result.contains("John%"));
        assert!(!result.contains("%John"));
    }

    #[test]
    fn render_ends_with_predicate() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Name".into(),
            column_index: 0,
            values: vec![val(
                &CellValue::String("son".into()),
                "son",
                WhereValueKind::String,
            )],
            operator: WhereOperator::EndsWith,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let result = render_predicate(&action).unwrap();
        assert!(result.contains("%son"));
    }

    /// Binary values support equality and membership (a BINARY(16) UUID
    /// key is the common case) but not text patterns or ranges.
    #[test]
    fn operator_availability_bytes_allows_equality_only() {
        let values = [val(
            &CellValue::Bytes(vec![1]),
            "0x01",
            WhereValueKind::Bytes,
        )];
        for operator in [
            WhereOperator::Equals,
            WhereOperator::NotEquals,
            WhereOperator::In,
            WhereOperator::NotIn,
            WhereOperator::IsNull,
            WhereOperator::IsNotNull,
        ] {
            let (avail, _) = operator_availability(operator, &values, WhereValueKind::Bytes, false);
            assert!(avail, "{operator:?}");
        }
        for operator in [
            WhereOperator::Like,
            WhereOperator::StartsWith,
            WhereOperator::GreaterThan,
            WhereOperator::Between,
        ] {
            let (avail, reason) =
                operator_availability(operator, &values, WhereValueKind::Bytes, false);
            assert!(!avail, "{operator:?}");
            assert!(reason.contains("Binary"), "{reason}");
        }
    }

    #[test]
    fn binary_equals_renders_the_engine_hex_literal() {
        let v = val(
            &CellValue::Bytes(vec![0xAB, 0x01]),
            "<2 bytes>",
            WhereValueKind::Bytes,
        );
        assert_eq!(
            single_sql_literal_for(DatabaseType::MySql, &v),
            Some("0xAB01".into())
        );
        let action = build_equals_action_for(DatabaseType::SqlServer, "Logo", 0, vec![v])
            .expect("binary equality builds");
        assert_eq!(action.predicate_preview, "[Logo] = 0xAB01");
        // The grid's placeholder never stands in for the bytes; text the
        // engine returned under a binary column compares as that text.
        let mut placeholder = val(
            &CellValue::String("<2 bytes>".into()),
            "<2 bytes>",
            WhereValueKind::Bytes,
        );
        placeholder.placeholder = true;
        assert_eq!(
            single_sql_literal_for(DatabaseType::MySql, &placeholder),
            None
        );
        let text = val(
            &CellValue::String("hello".into()),
            "hello",
            WhereValueKind::Bytes,
        );
        assert_eq!(
            single_sql_literal_for(DatabaseType::Sqlite, &text).as_deref(),
            Some("'hello'")
        );
    }

    /// Text under a binary column compares as text where the engine takes
    /// a text literal against bytes, and as the text's own bytes where it
    /// does not (SQL Server rejects nvarchar against varbinary; Oracle
    /// reads text against RAW as hex).
    #[test]
    fn text_under_a_binary_column_renders_per_engine() {
        let text = val(
            &CellValue::String("hello".into()),
            "hello",
            WhereValueKind::Bytes,
        );
        assert_eq!(
            single_sql_literal_for(DatabaseType::Sqlite, &text).as_deref(),
            Some("'hello'")
        );
        assert_eq!(
            single_sql_literal_for(DatabaseType::MySql, &text).as_deref(),
            Some("'hello'")
        );
        assert_eq!(
            single_sql_literal_for(DatabaseType::Postgres, &text).as_deref(),
            Some("'hello'")
        );
        // SQL Server and Oracle refuse rather than guess what the text
        // stands for: a base64 rendering spelled as its own bytes would be
        // a predicate that runs and matches nothing.
        assert_eq!(single_sql_literal_for(DatabaseType::SqlServer, &text), None);
        assert_eq!(
            single_sql_literal_for(DatabaseType::AzureSqlDatabase, &text),
            None
        );
        assert_eq!(single_sql_literal_for(DatabaseType::Oracle, &text), None);
        assert!(build_equals_action_for(DatabaseType::SqlServer, "Logo", 0, vec![text]).is_none());
        // Hex text still spells its bytes there.
        let hex = val(
            &CellValue::String("0x68656C6C6F".into()),
            "0x68656C6C6F",
            WhereValueKind::Bytes,
        );
        assert_eq!(
            single_sql_literal_for(DatabaseType::SqlServer, &hex).as_deref(),
            Some("0x68656C6C6F")
        );
    }

    /// With no engine known, `timestamp` is a date-time (only SQL Server
    /// makes it bytes) and `rowversion` is bytes everywhere.
    #[test]
    fn an_unknown_engine_reads_timestamp_as_a_date() {
        assert_eq!(
            kind_for_type_name(None, "timestamp"),
            Some(WhereValueKind::Date)
        );
        assert_eq!(
            kind_for_type_name(None, "rowversion"),
            Some(WhereValueKind::Bytes)
        );
        let text = CellValue::String("2024-05-01 10:00:00".into());
        assert_eq!(
            classify_value_for_type(None, &text, Some("timestamp")),
            WhereValueKind::Date
        );
    }

    #[test]
    fn classify_bytea_and_blob_columns_as_binary() {
        for data_type in ["bytea", "blob", "longblob", "varbinary(16)", "binary(16)"] {
            assert_eq!(
                classify_column(DatabaseType::SqlServer, &col("c", data_type)),
                WhereValueKind::Bytes,
                "{data_type}"
            );
            assert_eq!(
                classify_value(
                    DatabaseType::SqlServer,
                    &CellValue::String("<16 bytes>".into()),
                    Some(&col("c", data_type))
                ),
                WhereValueKind::Bytes,
                "{data_type}"
            );
        }
    }

    #[test]
    fn operator_availability_between_needs_two() {
        let (avail, reason) = operator_availability(
            WhereOperator::Between,
            &[val(&CellValue::I64(1), "1", WhereValueKind::Integer)],
            WhereValueKind::Integer,
            false,
        );
        assert!(!avail);
        assert!(reason.contains("two values"));
    }

    /// BETWEEN is the one range operator that takes two values; the
    /// single-value rule for the others must not refuse it.
    #[test]
    fn operator_availability_between_with_two_values() {
        let (avail, reason) = operator_availability(
            WhereOperator::Between,
            &[
                val(&CellValue::I64(1), "1", WhereValueKind::Integer),
                val(&CellValue::I64(9), "9", WhereValueKind::Integer),
            ],
            WhereValueKind::Integer,
            false,
        );
        assert!(avail, "{reason}");
    }

    #[test]
    fn operator_availability_empty_values() {
        let (avail, reason) =
            operator_availability(WhereOperator::Equals, &[], WhereValueKind::Integer, false);
        assert!(!avail);
        assert!(reason.contains("No selected values"));
    }

    #[test]
    fn operator_availability_ok() {
        let (avail, _reason) = operator_availability(
            WhereOperator::Equals,
            &[val(&CellValue::I64(42), "42", WhereValueKind::Integer)],
            WhereValueKind::Integer,
            false,
        );
        assert!(avail);
    }

    #[test]
    fn operator_availability_is_null_needs_no_value() {
        // IS NULL / IS NOT NULL are available without a selected value.
        for op in [WhereOperator::IsNull, WhereOperator::IsNotNull] {
            let (avail, _reason) = operator_availability(op, &[], WhereValueKind::String, false);
            assert!(avail, "{op:?} should be available with no value");
        }
    }

    #[test]
    fn operator_availability_comparison_single_value() {
        let (avail, _reason) = operator_availability(
            WhereOperator::GreaterThan,
            &[val(&CellValue::I64(1), "1", WhereValueKind::Integer)],
            WhereValueKind::Integer,
            false,
        );
        assert!(avail);
    }

    #[test]
    fn build_equals_action_integer() {
        let action = build_equals_action(
            "CustomerID",
            0,
            vec![val(&CellValue::I64(42), "42", WhereValueKind::Integer)],
        )
        .unwrap();
        assert_eq!(action.operator, WhereOperator::Equals);
        assert_eq!(action.predicate_preview, "[CustomerID] = 42");
    }

    #[test]
    fn build_equals_action_null_becomes_is_null() {
        let action = build_equals_action(
            "Email",
            0,
            vec![val(&CellValue::Null, "NULL", WhereValueKind::Null)],
        )
        .unwrap();
        assert_eq!(action.operator, WhereOperator::IsNull);
        assert_eq!(action.predicate_preview, "[Email] IS NULL");
    }

    #[test]
    fn build_in_action_multiple_values() {
        let action = build_in_action(
            "Status",
            0,
            vec![
                val(
                    &CellValue::String("Active".into()),
                    "Active",
                    WhereValueKind::String,
                ),
                val(
                    &CellValue::String("Pending".into()),
                    "Pending",
                    WhereValueKind::String,
                ),
                val(
                    &CellValue::String("Closed".into()),
                    "Closed",
                    WhereValueKind::String,
                ),
            ],
        )
        .unwrap();
        assert_eq!(action.operator, WhereOperator::In);
        assert!(action.predicate_preview.contains("N'Active'"));
        assert!(action.predicate_preview.contains("N'Closed'"));
    }

    #[test]
    fn apply_where_new_clause() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "CustomerID".into(),
            column_index: 0,
            values: vec![val(&CellValue::I64(42), "42", WhereValueKind::Integer)],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let result = apply_where_action("SELECT * FROM Customers", &action);
        assert!(result.updated_sql.contains("WHERE [CustomerID] = 42"));
    }

    #[test]
    fn apply_where_existing_clause_appends() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Status".into(),
            column_index: 0,
            values: vec![val(
                &CellValue::String("Active".into()),
                "Active",
                WhereValueKind::String,
            )],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let result = apply_where_action("SELECT * FROM Orders WHERE CustomerID = 42", &action);
        assert!(result.updated_sql.contains("[Status] = N'Active'"));
        assert!(result.updated_sql.contains("AND [Status]"));
    }

    #[test]
    fn apply_where_inserts_before_order_by() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Status".into(),
            column_index: 0,
            values: vec![val(
                &CellValue::String("Active".into()),
                "Active",
                WhereValueKind::String,
            )],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let result = apply_where_action("SELECT * FROM Orders ORDER BY CreatedAt", &action);
        assert!(result.updated_sql.contains("WHERE [Status] = N'Active'"));
        assert!(result.updated_sql.contains("ORDER BY CreatedAt"));
        // WHERE should come before ORDER BY
        let where_pos = result.updated_sql.find("WHERE").unwrap();
        let order_pos = result.updated_sql.find("ORDER BY").unwrap();
        assert!(where_pos < order_pos);
    }

    #[test]
    fn apply_where_copy_mode_no_change() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "X".into(),
            column_index: 0,
            values: vec![val(&CellValue::I64(1), "1", WhereValueKind::Integer)],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Copy,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let result = apply_where_action("SELECT * FROM T", &action);
        assert_eq!(result.updated_sql, "SELECT * FROM T");
    }

    #[test]
    fn find_where_keyword_basic() {
        let input = "SELECT * FROM T WHERE X = 1";
        let pos = find_where_keyword(input);
        assert!(pos.is_some());
        let pos = pos.unwrap();
        assert_eq!(&input[pos.start..pos.end], "WHERE");
    }

    #[test]
    fn find_where_keyword_none_when_absent() {
        assert!(find_where_keyword("SELECT * FROM T").is_none());
    }

    #[test]
    fn find_where_keyword_skips_strings() {
        let pos = find_where_keyword("SELECT * FROM T WHERE name = 'WHERE clause'");
        assert!(pos.is_some());
        // Should find the real WHERE, not the one in the string
        let pos = pos.unwrap();
        assert!(pos.start < 20);
    }

    #[test]
    fn find_where_keyword_case_insensitive() {
        assert!(find_where_keyword("SELECT * FROM T where X = 1").is_some());
        assert!(find_where_keyword("SELECT * FROM T Where X = 1").is_some());
    }

    #[test]
    fn render_predicate_for_postgres_quotes_and_literals() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Name".into(),
            column_index: 0,
            values: vec![val(
                &CellValue::String("O'Brien".into()),
                "O'Brien",
                WhereValueKind::String,
            )],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        assert_eq!(
            render_predicate_for(DatabaseType::Postgres, &action),
            Some("\"Name\" = 'O''Brien'".to_string())
        );
        assert_eq!(
            render_predicate_for(DatabaseType::MySql, &action),
            Some("`Name` = 'O''Brien'".to_string())
        );
    }

    #[test]
    fn render_predicate_for_postgres_boolean_uses_true() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "active".into(),
            column_index: 0,
            values: vec![val(&CellValue::Bool(true), "true", WhereValueKind::Bool)],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        assert_eq!(
            render_predicate_for(DatabaseType::Postgres, &action),
            Some("\"active\" = TRUE".to_string())
        );
        assert_eq!(
            render_predicate_for(DatabaseType::Sqlite, &action),
            Some("\"active\" = 1".to_string())
        );
    }

    #[test]
    fn render_like_for_postgres_has_no_bracket_escapes_or_n_prefix() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Notes".into(),
            column_index: 0,
            values: vec![val(
                &CellValue::String("50%_off[x]".into()),
                "50%_off[x]",
                WhereValueKind::String,
            )],
            operator: WhereOperator::Like,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let pg = render_predicate_for(DatabaseType::Postgres, &action).unwrap();
        // % and _ are escaped, brackets are NOT (invalid escape on PG). The
        // backslashes make both literals E'' strings, which read the same
        // whether standard_conforming_strings is on or off.
        assert_eq!(pg, r#""Notes" LIKE E'%50\\%\\_off[x]%' ESCAPE E'\\'"#);
        let mssql = render_predicate_for(DatabaseType::SqlServer, &action).unwrap();
        assert!(
            mssql.contains("N'%50\\%\\_off\\[x\\]%' ESCAPE '\\'"),
            "{mssql}"
        );
    }

    #[test]
    fn render_like_for_mysql_doubles_backslashes_in_literal() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Notes".into(),
            column_index: 0,
            values: vec![val(
                &CellValue::String("50%_off".into()),
                "50%_off",
                WhereValueKind::String,
            )],
            operator: WhereOperator::Like,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let my = render_predicate_for(DatabaseType::MySql, &action).unwrap();
        // The string literal doubles the escape backslashes so the LIKE sees
        // single ones; the ESCAPE literal is the doubled backslash form.
        assert_eq!(my, "`Notes` LIKE '%50\\\\%\\\\_off%' ESCAPE '\\\\'");
    }

    #[test]
    fn render_in_for_postgres_uses_plain_string_literals() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "Status".into(),
            column_index: 0,
            values: vec![
                val(
                    &CellValue::String("Active".into()),
                    "Active",
                    WhereValueKind::String,
                ),
                val(
                    &CellValue::String("Pending".into()),
                    "Pending",
                    WhereValueKind::String,
                ),
            ],
            operator: WhereOperator::In,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        assert_eq!(
            render_predicate_for(DatabaseType::Postgres, &action),
            Some("\"Status\" IN ('Active', 'Pending')".to_string())
        );
    }

    #[test]
    fn apply_where_action_for_renders_target_dialect() {
        let action = WhereAction {
            title: "test".into(),
            column_name: "name".into(),
            column_index: 0,
            values: vec![val(
                &CellValue::String("Ada".into()),
                "Ada",
                WhereValueKind::String,
            )],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let result = apply_where_action_for(DatabaseType::Postgres, "SELECT * FROM users", &action);
        assert!(
            result.updated_sql.contains("WHERE \"name\" = 'Ada'"),
            "{}",
            result.updated_sql
        );
    }

    #[test]
    fn apply_where_action_twice_produces_invalid_sql() {
        let action1 = WhereAction {
            title: "test".into(),
            column_name: "CustomerID".into(),
            column_index: 0,
            values: vec![val(&CellValue::I64(42), "42", WhereValueKind::Integer)],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let result1 = apply_where_action("SELECT * FROM Customers", &action1);
        assert!(
            result1.updated_sql.contains("WHERE [CustomerID] = 42"),
            "first call should insert a valid WHERE clause: {}",
            result1.updated_sql
        );

        let action2 = WhereAction {
            title: "test".into(),
            column_name: "Status".into(),
            column_index: 1,
            values: vec![val(
                &CellValue::String("Active".into()),
                "Active",
                WhereValueKind::String,
            )],
            operator: WhereOperator::Equals,
            mode: WhereActionMode::Add,
            join: WhereLogicalJoin::And,
            predicate_preview: String::new(),
            source: None,
        };
        let result2 = apply_where_action(&result1.updated_sql, &action2);

        let sql = &result2.updated_sql;
        let where_pos = sql.find("WHERE").unwrap();
        let after_where = sql[where_pos + 5..].trim_start();

        assert!(
            after_where.starts_with("AND"),
            "expected the bug: 'WHERE AND ...' (predicate inserted before existing \
             clause), but got: {sql}"
        );
        assert!(
            !after_where.starts_with("[CustomerID]"),
            "the first predicate should NOT be immediately after WHERE when a \
             second action is appended, but got: {sql}"
        );
    }
}
