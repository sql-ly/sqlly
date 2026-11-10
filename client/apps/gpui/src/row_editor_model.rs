//! Pure form model for the row editor. GPUI-free and fully unit-testable.
//!
//! Converts a resolved [`RowEditSession`] into per-field state, tracks drafts
//! and dirtiness (preserving NULL distinctly from empty string), runs
//! client-side prevalidation (nullability, type parse, string length, decimal
//! precision/scale, integer range), and assembles the guarded
//! [`UpdateRowRequest`] from dirty editable fields only.

use sqlly_core::{CellValue, DatabaseType};
use sqlly_services::row_editing::{
    ColumnExpression, DeleteRowRequest, InsertRowRequest, RowEditColumn, RowEditSession,
    UpdateRowRequest,
};

use crate::check_constraint::{CheckOutcome, evaluate_check, referenced_columns};

/// Whether the form edits an existing row or drafts a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditMode {
    Edit,
    Insert,
}

/// The user's draft for one field: an explicit NULL, literal text, a
/// non-value expression, or raw bytes (binary columns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldDraft {
    Null,
    Text(String),
    Expression(ColumnExpression),
    Bytes(Vec<u8>),
}

/// Broad input categories used to parse and validate drafts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Integer {
        min: i64,
        max: i64,
    },
    Decimal,
    Float,
    Bool,
    Guid,
    Date,
    Time,
    DateTime,
    Text,
    /// Binary, displayed as a summary; writes go through Load from file / NULL / DEFAULT.
    Binary,
    /// UDT, variant, spatial, … — displayed but not editable in v1.
    Unsupported,
}

/// Classifies a column type name into a [`FieldKind`]. Covers the four
/// engines' spellings in one map: SQL Server catalog names, PostgreSQL
/// `pg_type` names (plus the verbose `information_schema` forms), MySQL/
/// MariaDB `DATA_TYPE` names (with `tinyint(1)` preserved as the boolean
/// spelling), and SQLite declared-type bases. Identical names keep one
/// meaning; SQL Server classifications are unchanged.
pub fn field_kind(data_type: &str) -> FieldKind {
    match data_type.to_ascii_lowercase().as_str() {
        // MySQL's boolean spelling must win over plain tinyint.
        "tinyint(1)" | "bit" | "bool" | "boolean" => FieldKind::Bool,
        "tinyint" => FieldKind::Integer { min: 0, max: 255 },
        "smallint" | "int2" => FieldKind::Integer {
            min: i16::MIN as i64,
            max: i16::MAX as i64,
        },
        "mediumint" => FieldKind::Integer {
            min: -8_388_608,
            max: 8_388_607,
        },
        "int" | "int4" => FieldKind::Integer {
            min: i32::MIN as i64,
            max: i32::MAX as i64,
        },
        // SQLite INTEGER is a full 64-bit storage class.
        "bigint" | "int8" | "integer" => FieldKind::Integer {
            min: i64::MIN,
            max: i64::MAX,
        },
        "year" => FieldKind::Integer {
            min: 1901,
            max: 2155,
        },
        "decimal" | "numeric" => FieldKind::Decimal,
        "float" | "real" | "money" | "smallmoney" | "double" | "double precision" | "float4"
        | "float8" => FieldKind::Float,
        "uniqueidentifier" | "uuid" => FieldKind::Guid,
        "date" => FieldKind::Date,
        "time" | "timetz" | "time without time zone" | "time with time zone" => FieldKind::Time,
        "datetime"
        | "datetime2"
        | "smalldatetime"
        | "datetimeoffset"
        | "timestamp"
        | "timestamptz"
        | "timestamp without time zone"
        | "timestamp with time zone" => FieldKind::DateTime,
        "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" | "xml" | "sysname"
        | "character" | "character varying" | "bpchar" | "citext" | "name" | "tinytext"
        | "mediumtext" | "longtext" | "enum" | "set" | "json" | "jsonb" => FieldKind::Text,
        // SQLite columns may be declared with no type at all; any value is
        // storable, so edit them as text.
        "" => FieldKind::Text,
        "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary"
        | "image" => FieldKind::Binary,
        _ => FieldKind::Unsupported,
    }
}

/// Engine-aware variant of [`field_kind`]. MySQL/MariaDB default `TINYINT` is
/// signed (`-128..=127`); spellings containing `unsigned` stay `0..=255`.
/// `tinyint(1)` remains boolean. All other engines delegate to [`field_kind`]
/// (SQL Server `tinyint` stays unsigned).
pub fn field_kind_for(db: DatabaseType, data_type: &str) -> FieldKind {
    if matches!(db, DatabaseType::MySql | DatabaseType::MariaDb) {
        let lower = data_type.to_ascii_lowercase();
        return match lower.as_str() {
            "tinyint(1)" => FieldKind::Bool,
            s if s.contains("tinyint") && s.contains("unsigned") => {
                FieldKind::Integer { min: 0, max: 255 }
            }
            "tinyint" => FieldKind::Integer {
                min: -128,
                max: 127,
            },
            s if s.starts_with("tinyint(") => FieldKind::Integer {
                min: -128,
                max: 127,
            },
            _ => field_kind(data_type),
        };
    }
    field_kind(data_type)
}

/// Renders a cell value as editable text. `None` means NULL.
pub fn cell_to_draft(cell: &CellValue) -> FieldDraft {
    match cell {
        CellValue::Null => FieldDraft::Null,
        CellValue::Bool(b) => FieldDraft::Text(if *b { "true" } else { "false" }.to_string()),
        CellValue::I64(n) => FieldDraft::Text(n.to_string()),
        CellValue::F64(f) => FieldDraft::Text(f.to_string()),
        CellValue::String(s) => FieldDraft::Text(s.clone()),
        CellValue::Bytes(b) => FieldDraft::Bytes(b.clone()),
    }
}

/// State of one form field.
#[derive(Debug, Clone)]
pub struct FieldState {
    pub column: RowEditColumn,
    pub kind: FieldKind,
    pub original: CellValue,
    pub draft: FieldDraft,
    pub error: Option<String>,
    /// Index into [`RowEditSession::foreign_keys`] when this column
    /// participates in a foreign key.
    pub fk: Option<usize>,
    /// True for the first local column of its FK — the column that renders the
    /// searchable autocomplete and structurally fills its siblings on pick.
    pub fk_lead: bool,
    /// Display label for the current FK value (single-column lead FKs). Cleared
    /// when the user types a raw value.
    pub fk_label: Option<String>,
    /// True when this field belongs to an insert form. Insert relaxes the
    /// primary-key read-only rule (a non-identity PK must be supplied) but
    /// still treats identity/computed/rowversion as database-generated.
    pub for_insert: bool,
}

impl FieldState {
    /// Whether the editor allows writing this field: the column must be
    /// writable per catalog metadata AND of a type the v1 editor can parse.
    pub fn is_editable(&self) -> bool {
        if self.kind == FieldKind::Unsupported {
            return false;
        }
        // Binary is writable via file / NULL / DEFAULT, not as free-text hex.
        if self.for_insert {
            // On insert, only database-generated columns are off-limits; a
            // non-identity primary key must be user-supplied.
            !(self.column.is_identity || self.column.is_computed || self.column.is_rowversion)
        } else {
            self.column.is_editable()
        }
    }

    /// Whether the field must be supplied on insert (NOT NULL, no default, not
    /// database-generated).
    pub fn is_required_for_insert(&self) -> bool {
        self.for_insert
            && self.is_editable()
            && !self.column.nullable
            && self.column.default_definition.is_none()
    }

    /// Why the field is read-only, for display next to the label.
    pub fn read_only_reason(&self) -> Option<String> {
        column_read_only_reason(&self.column, self.kind, self.for_insert)
    }

    pub fn is_dirty(&self) -> bool {
        self.draft != cell_to_draft(&self.original)
    }

    /// Short type summary for the field's metadata line, e.g.
    /// `nvarchar(50), null` or `decimal(10,2), not null`.
    pub fn type_summary(&self) -> String {
        let base = &self.column.data_type;
        let sized = match self.kind {
            FieldKind::Text => match self.column.char_max_length {
                Some(n) => format!("{base}({n})"),
                None if matches!(
                    base.to_ascii_lowercase().as_str(),
                    "varchar" | "nvarchar" | "char" | "nchar"
                ) =>
                {
                    format!("{base}(max)")
                }
                None => base.clone(),
            },
            FieldKind::Decimal => match (self.column.precision, self.column.scale) {
                (Some(p), Some(s)) => format!("{base}({p},{s})"),
                _ => base.clone(),
            },
            _ => base.clone(),
        };
        crate::localization::format(
            "results.row_editor.type_summary",
            &[
                ("data_type", &sized),
                (
                    "nullability",
                    &crate::localization::text(if self.column.nullable {
                        "results.row_editor.nullable"
                    } else {
                        "results.row_editor.not_nullable"
                    }),
                ),
            ],
        )
    }

    /// Validates the current draft. `None` means valid.
    pub fn validation_error(&self) -> Option<String> {
        if !self.is_editable() {
            return None;
        }
        // On insert, a required column left blank is an error even before the
        // user touches it.
        if self.is_required_for_insert() {
            let blank = match &self.draft {
                FieldDraft::Null => true,
                FieldDraft::Text(text) => text.is_empty(),
                FieldDraft::Expression(_) | FieldDraft::Bytes(_) => false,
            };
            if blank {
                return Some(crate::localization::text(
                    "results.row_editor.required_for_insert",
                ));
            }
        }
        if !self.is_dirty() {
            return None;
        }
        match &self.draft {
            FieldDraft::Null => {
                if self.column.nullable {
                    None
                } else {
                    Some(crate::localization::text(
                        "results.row_editor.null_not_allowed",
                    ))
                }
            }
            FieldDraft::Text(text) => validate_text(text, self.kind, &self.column),
            FieldDraft::Expression(_) | FieldDraft::Bytes(_) => None,
        }
    }

    /// Converts the draft to the cell value sent to the server.
    /// Only call after validation passes.
    pub fn draft_cell(&self) -> CellValue {
        match &self.draft {
            FieldDraft::Null => CellValue::Null,
            FieldDraft::Text(text) => coerce_text(text, self.kind),
            FieldDraft::Expression(_) => self.original.clone(),
            FieldDraft::Bytes(bytes) => CellValue::Bytes(bytes.clone()),
        }
    }
}

/// Why `column` (of input `kind`) cannot be written, for the row editor's
/// field labels and the Cell Details panel's staged edits.
pub(crate) fn column_read_only_reason(
    column: &RowEditColumn,
    kind: FieldKind,
    for_insert: bool,
) -> Option<String> {
    if column.is_identity {
        Some(crate::localization::text(
            "results.row_editor.read_only_identity",
        ))
    } else if column.is_computed {
        Some(crate::localization::text(
            "results.row_editor.read_only_computed",
        ))
    } else if column.is_rowversion {
        Some(crate::localization::text(
            "results.row_editor.read_only_rowversion",
        ))
    } else if column.is_primary_key && !for_insert {
        Some(crate::localization::text(
            "results.row_editor.read_only_primary_key",
        ))
    } else if matches!(
        column.data_type.to_ascii_lowercase().as_str(),
        "geometry" | "geography"
    ) {
        Some(crate::localization::text(
            "results.row_editor.read_only_spatial",
        ))
    } else if kind == FieldKind::Unsupported {
        Some(crate::localization::text(
            "results.row_editor.read_only_unsupported_type",
        ))
    } else {
        None
    }
}

/// Parse draft text into a [`CellValue`] using the same rules as [`FieldState::draft_cell`].
pub(crate) fn coerce_text(text: &str, kind: FieldKind) -> CellValue {
    match kind {
        FieldKind::Integer { .. } => text
            .trim()
            .parse::<i64>()
            .map(CellValue::I64)
            .unwrap_or_else(|_| CellValue::String(text.to_string())),
        FieldKind::Float => text
            .trim()
            .parse::<f64>()
            .map(CellValue::F64)
            .unwrap_or_else(|_| CellValue::String(text.to_string())),
        FieldKind::Bool => match parse_bool(text) {
            Some(b) => CellValue::Bool(b),
            None => CellValue::String(text.to_string()),
        },
        // Decimal, GUID, temporal and text values travel as strings;
        // SQL Server converts them under the column's declared type.
        _ => CellValue::String(text.to_string()),
    }
}

fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

/// Digit-counting decimal validation honoring precision/scale, tolerating
/// sign, leading zeros, and trailing fractional zeros.
fn validate_decimal(text: &str, precision: u8, scale: u8) -> Option<String> {
    let trimmed = text.trim();
    let unsigned = trimmed
        .strip_prefix('-')
        .or_else(|| trimmed.strip_prefix('+'))
        .unwrap_or(trimmed);
    if unsigned.is_empty() {
        return Some(crate::localization::text(
            "results.row_editor.numeric_required",
        ));
    }
    let (int_part, frac_part) = match unsigned.split_once('.') {
        Some((i, f)) => (i, f),
        None => (unsigned, ""),
    };
    if int_part
        .chars()
        .chain(frac_part.chars())
        .any(|c| !c.is_ascii_digit())
        || (int_part.is_empty() && frac_part.is_empty())
    {
        return Some(crate::localization::text(
            "results.row_editor.numeric_required",
        ));
    }
    let int_digits = int_part.trim_start_matches('0').len();
    let frac_digits = frac_part.trim_end_matches('0').len();
    if frac_digits > scale as usize {
        return Some(crate::localization::format(
            "results.row_editor.too_many_decimal_places",
            &[("decimal_places", &scale.to_string())],
        ));
    }
    let max_int_digits = (precision - scale) as usize;
    if int_digits > max_int_digits {
        return Some(crate::localization::format(
            "results.row_editor.too_many_integer_digits",
            &[("integer_digits", &max_int_digits.to_string())],
        ));
    }
    None
}

fn is_guid(text: &str) -> bool {
    let t = text.trim().trim_start_matches('{').trim_end_matches('}');
    let groups: Vec<&str> = t.split('-').collect();
    groups.len() == 5
        && [8usize, 4, 4, 4, 12]
            .iter()
            .zip(&groups)
            .all(|(len, group)| group.len() == *len && group.chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_lexical_date(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    parts.len() == 3
        && parts[0].len() == 4
        && (1..=2).contains(&parts[1].len())
        && (1..=2).contains(&parts[2].len())
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
        && (1..=12).contains(&parts[1].parse::<u32>().unwrap_or(0))
        && (1..=31).contains(&parts[2].parse::<u32>().unwrap_or(0))
}

fn is_lexical_time(text: &str) -> bool {
    let (hms, frac) = match text.split_once('.') {
        Some((h, f)) => (h, Some(f)),
        None => (text, None),
    };
    if let Some(frac) = frac
        && (frac.is_empty() || frac.len() > 7 || !frac.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    let parts: Vec<&str> = hms.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return false;
    }
    let nums: Option<Vec<u32>> = parts.iter().map(|p| p.parse::<u32>().ok()).collect();
    match nums {
        Some(nums) => nums[0] < 24 && nums[1] < 60 && nums.get(2).map(|s| *s < 60).unwrap_or(true),
        None => false,
    }
}

fn is_lexical_datetime(text: &str) -> bool {
    // Accept `date`, `date time`, and `dateTtime[offset]` (ISO-8601-ish).
    // Offsets/`Z` are tolerated loosely; the server remains the authority.
    let text = text.trim();
    let split_at = text.find([' ', 'T']);
    let Some(at) = split_at else {
        return is_lexical_date(text);
    };
    let (date, rest) = text.split_at(at);
    let rest = &rest[1..];
    if !is_lexical_date(date) {
        return false;
    }
    let time_end = rest
        .find(['+', 'Z', 'z'])
        .or_else(|| rest.rfind('-').filter(|i| *i > 7));
    let time = match time_end {
        Some(end) => &rest[..end],
        None => rest,
    };
    is_lexical_time(time.trim())
}

pub(crate) fn validate_text(text: &str, kind: FieldKind, column: &RowEditColumn) -> Option<String> {
    match kind {
        FieldKind::Integer { min, max } => match text.trim().parse::<i64>() {
            Ok(value) if (min..=max).contains(&value) => None,
            Ok(_) => Some(crate::localization::format(
                "results.row_editor.integer_out_of_range",
                &[("minimum", &min.to_string()), ("maximum", &max.to_string())],
            )),
            Err(_) => Some(crate::localization::text(
                "results.row_editor.whole_number_required",
            )),
        },
        FieldKind::Decimal => match (column.precision, column.scale) {
            (Some(precision), Some(scale)) => validate_decimal(text, precision, scale),
            // No declared precision (PostgreSQL bare `numeric`, SQLite
            // NUMERIC affinity): accept any numeric text and let the server
            // stay the authority. SQL Server always declares both.
            _ => match text.trim().parse::<f64>() {
                Ok(value) if value.is_finite() => None,
                _ => Some(crate::localization::text(
                    "results.row_editor.numeric_required",
                )),
            },
        },
        FieldKind::Float => match text.trim().parse::<f64>() {
            Ok(value) if value.is_finite() => None,
            _ => Some(crate::localization::text(
                "results.row_editor.numeric_required",
            )),
        },
        FieldKind::Bool => match parse_bool(text) {
            Some(_) => None,
            None => Some(crate::localization::text(
                "results.row_editor.boolean_required",
            )),
        },
        FieldKind::Guid => {
            if is_guid(text) {
                None
            } else {
                Some(crate::localization::text(
                    "results.row_editor.guid_required",
                ))
            }
        }
        FieldKind::Date => {
            if is_lexical_date(text.trim()) {
                None
            } else {
                Some(crate::localization::text(
                    "results.row_editor.date_required",
                ))
            }
        }
        FieldKind::Time => {
            if is_lexical_time(text.trim()) {
                None
            } else {
                Some(crate::localization::text(
                    "results.row_editor.time_required",
                ))
            }
        }
        FieldKind::DateTime => {
            if is_lexical_datetime(text) {
                None
            } else {
                Some(crate::localization::text(
                    "results.row_editor.datetime_required",
                ))
            }
        }
        FieldKind::Text => match column.char_max_length {
            Some(max) if text.chars().count() as i64 > max => Some(crate::localization::format(
                "results.row_editor.text_too_long",
                &[
                    ("actual_length", &text.chars().count().to_string()),
                    ("maximum_length", &max.to_string()),
                ],
            )),
            _ => None,
        },
        FieldKind::Binary => Some(crate::localization::text(
            "results.row_editor.binary_load_required",
        )),
        FieldKind::Unsupported => Some(crate::localization::text(
            "results.row_editor.unsupported_type",
        )),
    }
}

/// A locally-detected CHECK-constraint violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckViolation {
    pub name: String,
    /// Columns the constraint references (message is attached to each).
    pub columns: Vec<String>,
    pub message: String,
}

/// The whole form: one field per table column, in catalog order.
#[derive(Debug, Clone)]
pub struct RowEditorModel {
    pub session: RowEditSession,
    pub fields: Vec<FieldState>,
}

impl RowEditorModel {
    pub fn new(session: RowEditSession, database_type: DatabaseType) -> Self {
        Self::build(session, EditMode::Edit, None, database_type, false)
    }

    /// Builds an insert form. `seed` (a source row aligned with catalog
    /// columns) prefills editable columns for "duplicate row"; identity,
    /// computed, and rowversion columns are never seeded.
    pub fn new_insert(
        session: RowEditSession,
        seed: Option<Vec<CellValue>>,
        database_type: DatabaseType,
    ) -> Self {
        Self::build(session, EditMode::Insert, seed, database_type, false)
    }

    /// Duplicate a row with primary-key columns cleared (still editable).
    pub fn new_insert_without_key(
        session: RowEditSession,
        seed: Option<Vec<CellValue>>,
        database_type: DatabaseType,
    ) -> Self {
        Self::build(session, EditMode::Insert, seed, database_type, true)
    }

    fn build(
        session: RowEditSession,
        mode: EditMode,
        seed: Option<Vec<CellValue>>,
        database_type: DatabaseType,
        clear_keys: bool,
    ) -> Self {
        let for_insert = mode == EditMode::Insert;
        let fields = session
            .columns
            .iter()
            .enumerate()
            .zip(&session.original_row)
            .map(|((ordinal, column), original)| {
                let (fk, fk_lead) = match session.foreign_key_for_column(&column.name) {
                    Some((fk_index, spec)) => {
                        let is_lead = spec
                            .column_pairs
                            .first()
                            .map(|(local, _)| local == &column.name)
                            .unwrap_or(false);
                        (Some(fk_index), is_lead)
                    }
                    None => (None, false),
                };
                let fk_label = if for_insert {
                    None
                } else {
                    fk.filter(|_| fk_lead)
                        .and_then(|fk_index| session.fk_display.get(fk_index).cloned().flatten())
                };
                // Insert: `original` is a NULL baseline so any supplied value
                // counts as dirty. A duplicate seed prefills settable columns.
                let db_generated = column.is_identity || column.is_computed || column.is_rowversion;
                let skip_seed = db_generated || (clear_keys && column.is_primary_key);
                let (original_cell, draft) = if for_insert {
                    let seeded = seed
                        .as_ref()
                        .filter(|_| !skip_seed)
                        .and_then(|row| row.get(ordinal).cloned());
                    match seeded {
                        Some(value) => (CellValue::Null, cell_to_draft(&value)),
                        None => (CellValue::Null, FieldDraft::Null),
                    }
                } else {
                    (original.clone(), cell_to_draft(original))
                };
                FieldState {
                    kind: field_kind_for(database_type, &column.data_type),
                    original: original_cell,
                    draft,
                    error: None,
                    column: column.clone(),
                    fk,
                    fk_lead,
                    fk_label,
                    for_insert,
                }
            })
            .collect();
        Self { session, fields }
    }

    pub fn set_text(&mut self, index: usize, text: String) {
        if let Some(field) = self.fields.get_mut(index) {
            field.draft = FieldDraft::Text(text);
            field.error = field.validation_error();
            // A hand-typed value invalidates any resolved FK display label.
            field.fk_label = None;
        }
    }

    /// Applies a chosen FK candidate: fills every local column of the foreign
    /// key from the referenced key (structural, so composite FKs stay
    /// consistent), and records the display label on the lead column.
    pub fn apply_fk_selection(
        &mut self,
        fk_index: usize,
        key: Vec<CellValue>,
        label: Option<String>,
    ) {
        let Some(spec) = self.session.foreign_keys.get(fk_index).cloned() else {
            return;
        };
        for (pos, (local, _referenced)) in spec.column_pairs.iter().enumerate() {
            let Some(value) = key.get(pos) else { continue };
            let Some(idx) = self.fields.iter().position(|f| &f.column.name == local) else {
                continue;
            };
            let field = &mut self.fields[idx];
            field.draft = cell_to_draft(value);
            field.error = field.validation_error();
            field.fk_label = if field.fk_lead { label.clone() } else { None };
        }
    }

    pub fn set_null(&mut self, index: usize, null: bool) {
        if let Some(field) = self.fields.get_mut(index) {
            field.draft = if null {
                FieldDraft::Null
            } else {
                // Leaving NULL starts from an empty string, kept distinct
                // from NULL from here on.
                FieldDraft::Text(String::new())
            };
            field.error = field.validation_error();
            field.fk_label = None;
        }
    }

    pub fn set_expression(&mut self, index: usize, expr: ColumnExpression) {
        if let Some(field) = self.fields.get_mut(index) {
            field.draft = FieldDraft::Expression(expr);
            field.error = field.validation_error();
            field.fk_label = None;
        }
    }

    pub fn set_bytes(&mut self, index: usize, bytes: Vec<u8>) {
        if let Some(field) = self.fields.get_mut(index) {
            field.draft = FieldDraft::Bytes(bytes);
            field.error = field.validation_error();
            field.fk_label = None;
        }
    }

    /// Reverts every field to the original row.
    pub fn reset(&mut self) {
        let fk_display = self.session.fk_display.clone();
        for field in &mut self.fields {
            field.draft = cell_to_draft(&field.original);
            field.error = None;
            // Restore the preloaded label for single-column lead FKs.
            field.fk_label = match (field.fk, field.fk_lead) {
                (Some(fk_index), true) => fk_display.get(fk_index).cloned().flatten(),
                _ => None,
            };
        }
    }

    /// Replaces the authoritative row (after refresh or save) and re-drafts.
    /// FK display labels are dropped: the new values may reference different
    /// rows, and the model does not perform lookups.
    pub fn adopt_row(&mut self, row: Vec<CellValue>) {
        if row.len() != self.fields.len() {
            return;
        }
        self.session.original_row = row.clone();
        self.session.fk_display = vec![None; self.session.foreign_keys.len()];
        for (field, value) in self.fields.iter_mut().zip(row) {
            field.original = value;
            field.draft = cell_to_draft(&field.original);
            field.error = None;
            field.fk_label = None;
        }
    }

    pub fn dirty_indices(&self) -> Vec<usize> {
        self.fields
            .iter()
            .enumerate()
            .filter(|(_, field)| field.is_editable() && field.is_dirty())
            .map(|(index, _)| index)
            .collect()
    }

    pub fn has_changes(&self) -> bool {
        !self.dirty_indices().is_empty()
    }

    /// Validates all dirty fields, records per-field errors, and returns
    /// whether the form can be saved. Runs type prevalidation first, then
    /// locally-decidable CHECK constraints touching a dirty field.
    pub fn validate(&mut self) -> bool {
        for field in &mut self.fields {
            field.error = field.validation_error();
        }
        for violation in self.check_violations() {
            for column in &violation.columns {
                if let Some(field) = self
                    .fields
                    .iter_mut()
                    .find(|f| f.column.name == *column && f.is_editable())
                    && field.error.is_none()
                {
                    field.error = Some(violation.message.clone());
                }
            }
        }
        self.fields.iter().all(|field| field.error.is_none())
    }

    /// Locally-decidable CHECK-constraint violations that involve a dirty
    /// field. Constraints the evaluator cannot judge are omitted (the server
    /// remains the authority), and pre-existing violations on untouched columns
    /// are ignored so the user is only told about problems they introduced.
    pub fn check_violations(&self) -> Vec<CheckViolation> {
        let mut violations = Vec::new();
        for check in &self.session.check_constraints {
            let columns = referenced_columns(&check.definition);
            if columns.is_empty() {
                continue;
            }
            let touches_dirty = columns.iter().any(|name| {
                self.fields
                    .iter()
                    .any(|f| &f.column.name == name && f.is_editable() && f.is_dirty())
            });
            if !touches_dirty {
                continue;
            }
            let resolve = |name: &str| -> Option<CellValue> {
                let field = self.fields.iter().find(|f| f.column.name == name)?;
                // An unparseable/invalid draft can't feed a reliable decision.
                if field.validation_error().is_some() {
                    return None;
                }
                Some(field.draft_cell())
            };
            if evaluate_check(&check.definition, &resolve) == CheckOutcome::Violated {
                violations.push(CheckViolation {
                    name: check.name.clone(),
                    columns,
                    message: crate::localization::format(
                        "results.row_editor.check_constraint_violation",
                        &[("constraint_name", &check.name)],
                    ),
                });
            }
        }
        violations
    }

    /// Assembles the guarded update request from dirty editable fields.
    /// Returns `None` when validation fails or nothing changed.
    pub fn build_update_request(&mut self) -> Option<UpdateRowRequest> {
        if !self.validate() || !self.has_changes() {
            return None;
        }
        let dirty = self.dirty_indices();
        let mut changes = Vec::new();
        let mut expressions = Vec::new();
        let original_values = dirty
            .iter()
            .map(|&index| {
                let field = &self.fields[index];
                match &field.draft {
                    FieldDraft::Expression(expr) => {
                        expressions.push((field.column.name.clone(), *expr));
                    }
                    _ => changes.push((field.column.name.clone(), field.draft_cell())),
                }
                (field.column.name.clone(), field.original.clone())
            })
            .collect();
        Some(UpdateRowRequest {
            target: self.session.target.clone(),
            key: self.session.key_values(),
            rowversion: self.session.rowversion_value(),
            changes,
            original_values,
            expressions,
        })
    }

    /// Assembles the insert request from supplied (non-default) fields. Returns
    /// `None` when validation fails. An empty value set inserts DEFAULT VALUES.
    pub fn build_insert_request(&mut self) -> Option<InsertRowRequest> {
        if !self.validate() {
            return None;
        }
        let mut values = Vec::new();
        let mut expressions = Vec::new();
        for field in &self.fields {
            if !(field.is_editable() && field.is_dirty()) {
                continue;
            }
            match &field.draft {
                FieldDraft::Expression(expr) => {
                    expressions.push((field.column.name.clone(), *expr));
                }
                _ => values.push((field.column.name.clone(), field.draft_cell())),
            }
        }
        Some(InsertRowRequest {
            target: self.session.target.clone(),
            values,
            expressions,
        })
    }

    /// Assembles a guarded delete for the current row (edit mode only).
    pub fn build_delete_request(&self) -> Option<DeleteRowRequest> {
        if self.session.key_columns.is_empty() {
            return None;
        }
        // Optimistic concurrency without a rowversion compares the original
        // values of comparable non-key columns.
        let original_values = self
            .session
            .columns
            .iter()
            .zip(&self.session.original_row)
            .filter(|(column, _)| {
                !self.session.key_columns.contains(&column.name)
                    && !column.is_rowversion
                    && concurrency_comparable(&column.data_type)
            })
            .map(|(column, value)| (column.name.clone(), value.clone()))
            .collect();
        Some(DeleteRowRequest {
            target: self.session.target.clone(),
            key: self.session.key_values(),
            rowversion: self.session.rowversion_value(),
            original_values,
        })
    }

    /// Attributes a server constraint-violation error to specific fields when
    /// the named constraint is a known CHECK or FOREIGN KEY of this table.
    /// Returns `true` when at least one field error was set.
    pub fn map_db_error_to_fields(&mut self, message: &str) -> bool {
        let Some(name) = extract_constraint_name(message) else {
            return false;
        };
        if let Some(check) = self
            .session
            .check_constraints
            .iter()
            .find(|c| c.name == name)
            .cloned()
        {
            let columns = referenced_columns(&check.definition);
            let mut mapped = false;
            for column in columns {
                if let Some(field) = self
                    .fields
                    .iter_mut()
                    .find(|f| f.column.name == column && f.is_editable())
                {
                    field.error = Some(crate::localization::format(
                        "results.row_editor.check_constraint_violation",
                        &[("constraint_name", &name)],
                    ));
                    mapped = true;
                }
            }
            return mapped;
        }
        if let Some(fk) = self
            .session
            .foreign_keys
            .iter()
            .find(|f| f.constraint_name == name)
            .cloned()
        {
            let referenced = fk.referenced.quoted();
            let mut mapped = false;
            for (local, _) in &fk.column_pairs {
                if let Some(field) = self
                    .fields
                    .iter_mut()
                    .find(|f| &f.column.name == local && f.is_editable())
                {
                    field.error = Some(crate::localization::format(
                        "results.row_editor.foreign_key_no_match",
                        &[("referenced_table", &referenced)],
                    ));
                    mapped = true;
                }
            }
            return mapped;
        }
        false
    }
}

/// Whether a column's type can safely appear in an equality predicate for
/// optimistic-concurrency checks. LOB and CLR types cannot be compared with
/// `=`, and PostgreSQL's non-binary `json` has no equality operator (`jsonb`
/// does).
fn concurrency_comparable(data_type: &str) -> bool {
    !matches!(
        data_type.to_ascii_lowercase().as_str(),
        "text"
            | "ntext"
            | "image"
            | "xml"
            | "geography"
            | "geometry"
            | "hierarchyid"
            | "sql_variant"
            | "json"
    )
}

/// Extracts the constraint name from a SQL Server constraint-violation message,
/// e.g. `... conflicted with the CHECK constraint "CK_Age". ...` → `CK_Age`.
fn extract_constraint_name(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    let at = lower.find("constraint")?;
    let after = &message[at + "constraint".len()..];
    let open = after.find('"')?;
    let rest = &after[open + 1..];
    let close = rest.find('"')?;
    Some(rest[..close].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::DatabaseType;
    use sqlly_services::row_editing::TableTarget;

    fn col(name: &str, data_type: &str) -> RowEditColumn {
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

    fn session(columns: Vec<RowEditColumn>, row: Vec<CellValue>) -> RowEditSession {
        RowEditSession {
            target: TableTarget {
                schema: "dbo".into(),
                table: "T".into(),
            },
            key_columns: vec![columns.first().map(|c| c.name.clone()).unwrap_or_default()],
            rowversion_column: None,
            columns,
            original_row: row,
            foreign_keys: Vec::new(),
            fk_display: Vec::new(),
            check_constraints: Vec::new(),
            incoming_foreign_keys: Vec::new(),
        }
    }

    fn model_with(column: RowEditColumn, original: CellValue) -> RowEditorModel {
        let mut key = col("Id", "int");
        key.is_primary_key = true;
        key.nullable = false;
        RowEditorModel::new(
            session(vec![key, column], vec![CellValue::I64(1), original]),
            DatabaseType::SqlServer,
        )
    }

    #[test]
    fn null_and_empty_string_stay_distinct() {
        let mut model = model_with(col("A", "nvarchar"), CellValue::Null);
        assert_eq!(model.fields[1].draft, FieldDraft::Null);
        assert!(!model.fields[1].is_dirty());

        model.set_null(1, false);
        assert_eq!(model.fields[1].draft, FieldDraft::Text(String::new()));
        assert!(model.fields[1].is_dirty(), "empty string differs from NULL");
        assert_eq!(
            model.fields[1].draft_cell(),
            CellValue::String(String::new())
        );

        model.set_null(1, true);
        assert!(!model.fields[1].is_dirty());
    }

    #[test]
    fn non_nullable_field_rejects_null() {
        let mut column = col("A", "nvarchar");
        column.nullable = false;
        let mut model = model_with(column, CellValue::String("x".into()));
        model.set_null(1, true);
        assert!(!model.validate());
        assert!(
            model.fields[1]
                .error
                .as_deref()
                .unwrap()
                .contains("required")
        );
    }

    #[test]
    fn read_only_fields_are_never_dirty_via_dirty_indices() {
        let mut column = col("A", "int");
        column.is_identity = true;
        let mut model = model_with(column, CellValue::I64(5));
        model.set_text(1, "6".into());
        assert!(model.dirty_indices().is_empty(), "identity is read-only");
        assert!(model.build_update_request().is_none());
    }

    #[test]
    fn primary_key_is_read_only() {
        let model = model_with(col("A", "int"), CellValue::I64(5));
        assert!(!model.fields[0].is_editable());
        assert_eq!(
            model.fields[0].read_only_reason(),
            Some("primary key".to_string())
        );
    }

    #[test]
    fn integer_range_by_type() {
        let mut model = model_with(col("A", "tinyint"), CellValue::I64(1));
        model.set_text(1, "256".into());
        assert!(!model.validate());
        model.set_text(1, "255".into());
        assert!(model.validate());
        model.set_text(1, "abc".into());
        assert!(!model.validate());
    }

    #[test]
    fn decimal_precision_and_scale_edges() {
        let mut column = col("A", "decimal");
        column.precision = Some(5);
        column.scale = Some(2);
        let mut model = model_with(column, CellValue::String("1.00".into()));

        model.set_text(1, "123.45".into());
        assert!(model.validate());
        model.set_text(1, "999.9".into());
        assert!(model.validate());
        model.set_text(1, "-000123.4500".into());
        assert!(
            model.validate(),
            "sign, leading and trailing zeros tolerated"
        );
        model.set_text(1, "1.234".into());
        assert!(!model.validate(), "too many decimal places");
        model.set_text(1, "1234.56".into());
        assert!(!model.validate(), "too many integer digits");
        model.set_text(1, "12x".into());
        assert!(!model.validate());
    }

    #[test]
    fn string_length_limit_counts_characters() {
        let mut column = col("A", "nvarchar");
        column.char_max_length = Some(3);
        let mut model = model_with(column, CellValue::String("ab".into()));
        model.set_text(1, "abc".into());
        assert!(model.validate());
        model.set_text(1, "abcd".into());
        assert!(!model.validate());
        model.set_text(1, "äöü".into());
        assert!(model.validate(), "characters, not bytes");
    }

    #[test]
    fn max_types_have_no_limit() {
        let column = col("A", "nvarchar"); // char_max_length None == max
        let mut model = model_with(column, CellValue::String("x".into()));
        model.set_text(1, "y".repeat(10_000));
        assert!(model.validate());
        assert_eq!(model.fields[1].type_summary(), "nvarchar(max), null");
    }

    #[test]
    fn bool_parse_and_cell() {
        let mut model = model_with(col("A", "bit"), CellValue::Bool(false));
        model.set_text(1, "maybe".into());
        assert!(!model.validate());
        model.set_text(1, "1".into());
        assert!(model.validate());
        assert_eq!(model.fields[1].draft_cell(), CellValue::Bool(true));
    }

    #[test]
    fn guid_and_temporal_lexical_parse() {
        let mut model = model_with(col("A", "uniqueidentifier"), CellValue::Null);
        model.set_text(1, "0BB60E32-7EAD-4B12-A132-D9D2BB57A25A".into());
        assert!(model.validate());
        model.set_text(1, "not-a-guid".into());
        assert!(!model.validate());

        let mut model = model_with(col("A", "date"), CellValue::Null);
        model.set_text(1, "2026-07-16".into());
        assert!(model.validate());
        model.set_text(1, "2026-13-16".into());
        assert!(!model.validate());

        let mut model = model_with(col("A", "time"), CellValue::Null);
        model.set_text(1, "13:30:00.123".into());
        assert!(model.validate());
        model.set_text(1, "25:00".into());
        assert!(!model.validate());

        let mut model = model_with(col("A", "datetime2"), CellValue::Null);
        model.set_text(1, "2026-07-16 13:30:00".into());
        assert!(model.validate());
        model.set_text(1, "2026-07-16T13:30:00".into());
        assert!(model.validate());
        model.set_text(1, "yesterday".into());
        assert!(!model.validate());
    }

    #[test]
    fn binary_columns_are_editable_via_file() {
        let model = model_with(col("A", "varbinary"), CellValue::Bytes(vec![1, 2]));
        assert!(model.fields[1].is_editable());
        assert_eq!(model.fields[1].kind, FieldKind::Binary);
        assert_eq!(model.fields[1].draft, FieldDraft::Bytes(vec![1, 2]));
    }

    #[test]
    fn unsupported_types_are_read_only() {
        let model = model_with(col("A", "geography"), CellValue::Null);
        assert!(!model.fields[1].is_editable());
        assert_eq!(
            model.fields[1].read_only_reason(),
            Some("spatial type — view only".to_string())
        );
    }

    #[test]
    fn update_request_contains_only_dirty_editable_fields() {
        let mut key = col("Id", "int");
        key.is_primary_key = true;
        key.nullable = false;
        let mut model = RowEditorModel::new(
            session(
                vec![key, col("A", "nvarchar"), col("B", "int")],
                vec![
                    CellValue::I64(7),
                    CellValue::String("old".into()),
                    CellValue::I64(3),
                ],
            ),
            DatabaseType::SqlServer,
        );
        model.set_text(1, "new".into());
        let request = model.build_update_request().expect("valid");
        assert_eq!(request.key, vec![("Id".to_string(), CellValue::I64(7))]);
        assert_eq!(
            request.changes,
            vec![("A".to_string(), CellValue::String("new".into()))]
        );
        assert_eq!(
            request.original_values,
            vec![("A".to_string(), CellValue::String("old".into()))]
        );
        assert!(request.rowversion.is_none());
    }

    #[test]
    fn no_request_when_nothing_changed_or_invalid() {
        let mut model = model_with(col("A", "int"), CellValue::I64(3));
        assert!(model.build_update_request().is_none(), "nothing dirty");
        model.set_text(1, "abc".into());
        assert!(model.build_update_request().is_none(), "invalid");
        assert!(model.fields[1].error.is_some());
    }

    #[test]
    fn reset_and_adopt_row_clear_drafts() {
        let mut model = model_with(col("A", "int"), CellValue::I64(3));
        model.set_text(1, "9".into());
        assert!(model.has_changes());
        model.reset();
        assert!(!model.has_changes());

        model.set_text(1, "9".into());
        model.adopt_row(vec![CellValue::I64(1), CellValue::I64(9)]);
        assert!(!model.has_changes());
        assert_eq!(model.fields[1].original, CellValue::I64(9));
    }

    #[test]
    fn float_original_draft_roundtrip_is_clean() {
        let model = model_with(col("A", "float"), CellValue::F64(3.5));
        assert!(!model.fields[1].is_dirty());
        assert_eq!(model.fields[1].draft, FieldDraft::Text("3.5".into()));
    }

    #[test]
    fn edited_numbers_ignore_the_os_locale() {
        // A German-locale user typing 2.5 must save 2.5, never 25.
        assert_eq!(coerce_text("2.5", FieldKind::Float), CellValue::F64(2.5));
        assert_eq!(
            coerce_text("2.5", FieldKind::Decimal),
            CellValue::String("2.5".into())
        );
        // A comma is not read as a decimal point or a grouping separator.
        assert_eq!(
            coerce_text("2,5", FieldKind::Float),
            CellValue::String("2,5".into())
        );
    }

    #[test]
    fn fk_lead_and_label_seed_from_session() {
        use sqlly_services::row_editing::ForeignKeySpec;
        let mut key = col("Id", "int");
        key.is_primary_key = true;
        key.nullable = false;
        let fk_col = col("CustomerId", "int");
        let mut session = session(
            vec![key, fk_col],
            vec![CellValue::I64(1), CellValue::I64(42)],
        );
        session.foreign_keys = vec![ForeignKeySpec {
            constraint_name: "FK_C".into(),
            referenced: TableTarget {
                schema: "dbo".into(),
                table: "Customer".into(),
            },
            column_pairs: vec![("CustomerId".into(), "Id".into())],
            display_column: Some("Name".into()),
        }];
        session.fk_display = vec![Some("Acme Corp".into())];
        let model = RowEditorModel::new(session, DatabaseType::SqlServer);
        assert_eq!(model.fields[1].fk, Some(0));
        assert!(model.fields[1].fk_lead);
        assert_eq!(model.fields[1].fk_label.as_deref(), Some("Acme Corp"));
        // Non-FK key column carries no FK metadata.
        assert_eq!(model.fields[0].fk, None);
    }

    #[test]
    fn apply_fk_selection_fills_and_typing_clears_label() {
        use sqlly_services::row_editing::ForeignKeySpec;
        let mut key = col("Id", "int");
        key.is_primary_key = true;
        key.nullable = false;
        let mut session = session(
            vec![key, col("CustomerId", "int")],
            vec![CellValue::I64(1), CellValue::I64(42)],
        );
        session.foreign_keys = vec![ForeignKeySpec {
            constraint_name: "FK_C".into(),
            referenced: TableTarget {
                schema: "dbo".into(),
                table: "Customer".into(),
            },
            column_pairs: vec![("CustomerId".into(), "Id".into())],
            display_column: Some("Name".into()),
        }];
        session.fk_display = vec![None];
        let mut model = RowEditorModel::new(session, DatabaseType::SqlServer);

        model.apply_fk_selection(0, vec![CellValue::I64(99)], Some("Beta LLC".into()));
        assert_eq!(model.fields[1].draft, FieldDraft::Text("99".into()));
        assert_eq!(model.fields[1].fk_label.as_deref(), Some("Beta LLC"));
        let request = model.build_update_request().expect("valid");
        assert_eq!(
            request.changes,
            vec![("CustomerId".to_string(), CellValue::I64(99))]
        );

        // Typing a raw value drops the stale label.
        model.set_text(1, "7".into());
        assert_eq!(model.fields[1].fk_label, None);
    }

    #[test]
    fn apply_fk_selection_fills_composite_members() {
        use sqlly_services::row_editing::ForeignKeySpec;
        let mut key = col("Id", "int");
        key.is_primary_key = true;
        key.nullable = false;
        let mut session = session(
            vec![key, col("RegionId", "int"), col("SubId", "int")],
            vec![CellValue::I64(1), CellValue::I64(2), CellValue::I64(3)],
        );
        session.foreign_keys = vec![ForeignKeySpec {
            constraint_name: "FK_Composite".into(),
            referenced: TableTarget {
                schema: "dbo".into(),
                table: "Region".into(),
            },
            column_pairs: vec![
                ("RegionId".into(), "RegionId".into()),
                ("SubId".into(), "SubId".into()),
            ],
            display_column: None,
        }];
        session.fk_display = vec![None];
        let mut model = RowEditorModel::new(session, DatabaseType::SqlServer);
        // Only the first member leads the autocomplete.
        assert!(model.fields[1].fk_lead);
        assert!(!model.fields[2].fk_lead);
        assert_eq!(model.fields[2].fk, Some(0));

        model.apply_fk_selection(0, vec![CellValue::I64(10), CellValue::I64(20)], None);
        assert_eq!(model.fields[1].draft, FieldDraft::Text("10".into()));
        assert_eq!(model.fields[2].draft, FieldDraft::Text("20".into()));
    }

    #[test]
    fn check_constraint_blocks_save_and_marks_field() {
        use sqlly_services::row_editing::CheckConstraintSpec;
        let mut key = col("Id", "int");
        key.is_primary_key = true;
        key.nullable = false;
        let mut session = session(
            vec![key, col("Age", "int")],
            vec![CellValue::I64(1), CellValue::I64(5)],
        );
        session.check_constraints = vec![CheckConstraintSpec {
            name: "CK_Age".into(),
            definition: "([Age]>=(0))".into(),
        }];
        let mut model = RowEditorModel::new(session, DatabaseType::SqlServer);
        // Untouched: no violation reported even though evaluator could run.
        assert!(model.check_violations().is_empty());

        model.set_text(1, "-3".into());
        assert!(!model.validate(), "negative age violates CK_Age");
        assert_eq!(
            model.fields[1].error.as_deref(),
            Some("violates constraint CK_Age")
        );
        assert!(model.build_update_request().is_none());

        model.set_text(1, "7".into());
        assert!(model.validate());
    }

    #[test]
    fn map_db_error_attributes_check_to_field() {
        use sqlly_services::row_editing::CheckConstraintSpec;
        let mut key = col("Id", "int");
        key.is_primary_key = true;
        key.nullable = false;
        let mut session = session(
            vec![key, col("Age", "int")],
            vec![CellValue::I64(1), CellValue::I64(5)],
        );
        session.check_constraints = vec![CheckConstraintSpec {
            name: "CK_Age".into(),
            definition: "([Age]>=(0))".into(),
        }];
        let mut model = RowEditorModel::new(session, DatabaseType::SqlServer);
        let message = "The UPDATE statement conflicted with the CHECK constraint \
                       \"CK_Age\". The conflict occurred in table \"dbo.Person\".";
        assert!(model.map_db_error_to_fields(message));
        assert_eq!(
            model.fields[1].error.as_deref(),
            Some("violates constraint CK_Age")
        );
    }

    #[test]
    fn map_db_error_attributes_fk_to_field() {
        use sqlly_services::row_editing::ForeignKeySpec;
        let mut key = col("Id", "int");
        key.is_primary_key = true;
        key.nullable = false;
        let mut session = session(
            vec![key, col("CustomerId", "int")],
            vec![CellValue::I64(1), CellValue::I64(5)],
        );
        session.foreign_keys = vec![ForeignKeySpec {
            constraint_name: "FK_Cust".into(),
            referenced: TableTarget {
                schema: "dbo".into(),
                table: "Customer".into(),
            },
            column_pairs: vec![("CustomerId".into(), "Id".into())],
            display_column: None,
        }];
        let mut model = RowEditorModel::new(session, DatabaseType::SqlServer);
        let message = "conflicted with the FOREIGN KEY constraint \"FK_Cust\".";
        assert!(model.map_db_error_to_fields(message));
        assert!(
            model.fields[1]
                .error
                .as_deref()
                .unwrap()
                .contains("Customer")
        );
    }

    #[test]
    fn insert_mode_flags_required_and_omits_unset() {
        let mut id = col("Id", "int");
        id.is_primary_key = true;
        id.is_identity = true;
        id.nullable = false;
        let mut name = col("Name", "nvarchar");
        name.nullable = false; // required, no default
        let note = col("Note", "nvarchar"); // nullable optional
        let session = session(
            vec![id, name, note],
            vec![CellValue::Null, CellValue::Null, CellValue::Null],
        );
        let mut model = RowEditorModel::new_insert(session, None, DatabaseType::SqlServer);
        // Identity is database-generated -> read-only even though it's the PK.
        assert!(!model.fields[0].is_editable());
        assert_eq!(
            model.fields[0].read_only_reason(),
            Some("identity".to_string())
        );
        // Name is required and blank -> invalid until supplied.
        assert!(!model.validate());
        assert!(model.fields[1].error.is_some());

        model.set_text(1, "Ada".into());
        assert!(model.validate());
        let request = model.build_insert_request().expect("valid");
        // Only the supplied Name is sent; identity + untouched Note are omitted.
        assert_eq!(
            request.values,
            vec![("Name".to_string(), CellValue::String("Ada".into()))]
        );
    }

    #[test]
    fn duplicate_seeds_settable_columns_only() {
        let mut id = col("Id", "int");
        id.is_primary_key = true;
        id.is_identity = true;
        id.nullable = false;
        let name = col("Name", "nvarchar");
        let session = session(vec![id, name], vec![CellValue::Null, CellValue::Null]);
        let seed = vec![CellValue::I64(99), CellValue::String("Grace".into())];
        let model = RowEditorModel::new_insert(session, Some(seed), DatabaseType::SqlServer);
        // Identity is not seeded.
        assert_eq!(model.fields[0].draft, FieldDraft::Null);
        // Regular column is prefilled and counts as dirty.
        assert_eq!(model.fields[1].draft, FieldDraft::Text("Grace".into()));
        assert!(model.fields[1].is_dirty());
    }

    #[test]
    fn duplicate_without_key_clears_primary_key_but_keeps_other_columns() {
        let mut code = col("code", "nvarchar");
        code.is_primary_key = true;
        code.nullable = false;
        let name = col("name", "nvarchar");
        let session = session(vec![code, name], vec![CellValue::Null, CellValue::Null]);
        let seed = vec![
            CellValue::String("ABC".into()),
            CellValue::String("Widget".into()),
        ];
        let model =
            RowEditorModel::new_insert_without_key(session, Some(seed), DatabaseType::SqlServer);
        assert_eq!(model.fields[0].draft, FieldDraft::Null);
        assert!(model.fields[0].is_editable());
        assert_eq!(model.fields[1].draft, FieldDraft::Text("Widget".into()));
    }

    #[test]
    fn delete_request_excludes_lob_and_key_from_concurrency() {
        let mut id = col("Id", "int");
        id.is_primary_key = true;
        id.nullable = false;
        let name = col("Name", "nvarchar");
        let blob = col("Doc", "xml");
        let mut model = RowEditorModel::new(
            session(
                vec![id, name, blob],
                vec![
                    CellValue::I64(7),
                    CellValue::String("x".into()),
                    CellValue::String("<a/>".into()),
                ],
            ),
            DatabaseType::SqlServer,
        );
        let _ = model.validate();
        let request = model.build_delete_request().expect("has key");
        assert_eq!(request.key, vec![("Id".to_string(), CellValue::I64(7))]);
        // Name participates in concurrency; the key and the XML LOB do not.
        assert_eq!(
            request.original_values,
            vec![("Name".to_string(), CellValue::String("x".into()))]
        );
    }

    #[test]
    fn type_summary_renders_length_and_precision() {
        let mut column = col("A", "decimal");
        column.precision = Some(10);
        column.scale = Some(2);
        column.nullable = false;
        let model = model_with(column, CellValue::Null);
        assert_eq!(model.fields[1].type_summary(), "decimal(10,2), not null");
    }

    #[test]
    fn field_kind_classifies_postgres_types() {
        assert_eq!(
            field_kind("int4"),
            FieldKind::Integer {
                min: i32::MIN as i64,
                max: i32::MAX as i64
            }
        );
        assert_eq!(
            field_kind("int8"),
            FieldKind::Integer {
                min: i64::MIN,
                max: i64::MAX
            }
        );
        assert_eq!(
            field_kind("int2"),
            FieldKind::Integer {
                min: i16::MIN as i64,
                max: i16::MAX as i64
            }
        );
        assert_eq!(field_kind("boolean"), FieldKind::Bool);
        assert_eq!(field_kind("bool"), FieldKind::Bool);
        assert_eq!(field_kind("character varying"), FieldKind::Text);
        assert_eq!(field_kind("bpchar"), FieldKind::Text);
        assert_eq!(field_kind("citext"), FieldKind::Text);
        assert_eq!(field_kind("numeric"), FieldKind::Decimal);
        assert_eq!(field_kind("float8"), FieldKind::Float);
        assert_eq!(field_kind("double precision"), FieldKind::Float);
        assert_eq!(field_kind("timestamptz"), FieldKind::DateTime);
        assert_eq!(field_kind("timestamp with time zone"), FieldKind::DateTime);
        assert_eq!(field_kind("timetz"), FieldKind::Time);
        assert_eq!(field_kind("uuid"), FieldKind::Guid);
        assert_eq!(field_kind("jsonb"), FieldKind::Text);
        assert_eq!(field_kind("bytea"), FieldKind::Binary);
    }

    #[test]
    fn field_kind_classifies_mysql_types() {
        assert_eq!(field_kind("tinyint(1)"), FieldKind::Bool);
        assert_eq!(
            field_kind_for(DatabaseType::MySql, "tinyint(1)"),
            FieldKind::Bool
        );
        assert_eq!(
            field_kind("tinyint"),
            FieldKind::Integer { min: 0, max: 255 }
        );
        assert_eq!(
            field_kind_for(DatabaseType::MySql, "tinyint"),
            FieldKind::Integer {
                min: -128,
                max: 127
            }
        );
        assert_eq!(
            field_kind_for(DatabaseType::MariaDb, "tinyint"),
            FieldKind::Integer {
                min: -128,
                max: 127
            }
        );
        assert_eq!(
            field_kind_for(DatabaseType::MySql, "tinyint unsigned"),
            FieldKind::Integer { min: 0, max: 255 }
        );
        assert_eq!(
            field_kind_for(DatabaseType::MySql, "tinyint(4) unsigned"),
            FieldKind::Integer { min: 0, max: 255 }
        );
        assert_eq!(
            field_kind("mediumint"),
            FieldKind::Integer {
                min: -8_388_608,
                max: 8_388_607
            }
        );
        assert_eq!(
            field_kind("year"),
            FieldKind::Integer {
                min: 1901,
                max: 2155
            }
        );
        assert_eq!(field_kind("varchar"), FieldKind::Text);
        assert_eq!(field_kind("longtext"), FieldKind::Text);
        assert_eq!(field_kind("enum"), FieldKind::Text);
        assert_eq!(field_kind("json"), FieldKind::Text);
        assert_eq!(field_kind("datetime"), FieldKind::DateTime);
        assert_eq!(field_kind("timestamp"), FieldKind::DateTime);
        assert_eq!(field_kind("decimal"), FieldKind::Decimal);
        assert_eq!(field_kind("double"), FieldKind::Float);
        assert_eq!(field_kind("blob"), FieldKind::Binary);
        assert_eq!(field_kind("varbinary"), FieldKind::Binary);
    }

    #[test]
    fn field_kind_classifies_sqlite_types() {
        assert_eq!(
            field_kind("integer"),
            FieldKind::Integer {
                min: i64::MIN,
                max: i64::MAX
            }
        );
        assert_eq!(field_kind("text"), FieldKind::Text);
        assert_eq!(field_kind("real"), FieldKind::Float);
        assert_eq!(field_kind("numeric"), FieldKind::Decimal);
        assert_eq!(field_kind("blob"), FieldKind::Binary);
        // Typeless SQLite columns accept anything; edit them as text.
        assert_eq!(field_kind(""), FieldKind::Text);
    }

    #[test]
    fn field_kind_keeps_sql_server_classifications() {
        assert_eq!(field_kind("bit"), FieldKind::Bool);
        assert_eq!(
            field_kind("tinyint"),
            FieldKind::Integer { min: 0, max: 255 }
        );
        assert_eq!(
            field_kind_for(DatabaseType::SqlServer, "tinyint"),
            FieldKind::Integer { min: 0, max: 255 }
        );
        assert_eq!(field_kind("uniqueidentifier"), FieldKind::Guid);
        assert_eq!(field_kind("nvarchar"), FieldKind::Text);
        assert_eq!(field_kind("datetime2"), FieldKind::DateTime);
        assert_eq!(field_kind("varbinary"), FieldKind::Binary);
        assert_eq!(field_kind("geography"), FieldKind::Unsupported);
        assert_eq!(field_kind("geometry"), FieldKind::Unsupported);
    }

    #[test]
    fn read_only_reason_names_spatial_types() {
        let model = model_with(col("G", "geometry"), CellValue::String("POINT(1 2)".into()));
        assert_eq!(
            model.fields[1].read_only_reason(),
            Some("spatial type — view only".to_string())
        );
        let model = model_with(
            col("G", "geography"),
            CellValue::String("POINT(1 2)".into()),
        );
        assert_eq!(
            model.fields[1].read_only_reason(),
            Some("spatial type — view only".to_string())
        );
    }

    #[test]
    fn decimal_without_declared_precision_accepts_any_numeric_text() {
        // PostgreSQL bare `numeric` / SQLite NUMERIC affinity: no declared
        // precision, so digit budgeting is impossible — numeric parse only.
        let mut model = model_with(col("A", "numeric"), CellValue::String("1".into()));
        model.set_text(1, "12345.6789".into());
        assert!(model.validate());
        model.set_text(1, "-0.5".into());
        assert!(model.validate());
        model.set_text(1, "abc".into());
        assert!(!model.validate());
    }
}
