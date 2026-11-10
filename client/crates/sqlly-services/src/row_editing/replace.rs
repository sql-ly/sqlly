//! Grid find & replace → staged-edit mapping (pure).
//!
//! The app side resolves the find bar's matches to source rows and a row-edit
//! session; this module decides which of those matched cells may be written
//! and what the replacement value is, so the rules stay unit-testable without
//! any gpui types. The host stages the resulting edits through the normal
//! pending-changes pipeline, so replacements preview and save in the same
//! transaction as manual edits.

use sqlly_core::{CellValue, ResultColumn};

use super::types::{RowEditColumn, RowEditSession};

/// One planned cell write from find & replace: the row's position in the
/// caller's matched-row list, the column name, and the replacement value.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplaceEdit {
    pub row: usize,
    pub column: String,
    pub value: CellValue,
}

/// Whether find & replace may write `column`: it must be an editable table
/// column (not identity/computed/rowversion/key) and plain text. Enum-valued
/// columns are excluded — an arbitrary replacement can violate the value
/// constraint.
pub fn column_replaceable(session: &RowEditSession, column: &str) -> bool {
    session
        .columns
        .iter()
        .any(|c| c.name == column && is_replaceable(c))
}

fn is_replaceable(column: &RowEditColumn) -> bool {
    column.is_editable() && is_plain_text_column(column)
}

/// Whether free-text rewrites (find & replace, the transform macros) may be
/// applied to `column`: a plain character type with no enum constraint.
/// JSON/XML, MySQL `ENUM`/`SET`, and user-defined types are excluded — a
/// case or diacritic rewrite of those is more likely to corrupt the value (or,
/// on MySQL in non-strict mode, silently store `''`) than to help.
pub fn is_plain_text_column(column: &RowEditColumn) -> bool {
    column.enum_values.is_empty() && is_plain_text_type(&column.data_type)
}

/// The table column behind result column `result_index`, resolved through
/// the session's provenance (`result_ordinal`) only, so aliased projections
/// (`SELECT name AS n`) map to `name`. There is deliberately no fallback to
/// a same-name match: in `SELECT id, left(body, 50) AS body` the result
/// column `body` is an expression, and writing its (truncated) text into the
/// real `body` column would destroy data. A result column without provenance
/// is not editable.
pub fn table_column_for_result(
    session: &RowEditSession,
    result_index: usize,
) -> Option<&RowEditColumn> {
    session
        .columns
        .iter()
        .find(|c| c.result_ordinal == Some(result_index))
}

/// A result row re-aligned to `session.columns` (the shape staged originals
/// use for optimistic-concurrency checks). Projected columns come from the
/// result row via provenance; unprojected ones (including columns whose
/// result-set name only *looks* like theirs, e.g. an aliased expression) are
/// unknown and read NULL — except for the session's own row, whose fetched
/// values (including an unprojected rowversion) are authoritative and
/// returned as-is.
pub fn session_aligned_row(session: &RowEditSession, result_row: &[CellValue]) -> Vec<CellValue> {
    let aligned: Vec<CellValue> = session
        .columns
        .iter()
        .map(|column| {
            column
                .result_ordinal
                .and_then(|ordinal| result_row.get(ordinal).cloned())
                .unwrap_or(CellValue::Null)
        })
        .collect();
    let is_session_row = !session.key_columns.is_empty()
        && session.original_row.len() == aligned.len()
        && session.key_columns.iter().all(|key| {
            session
                .columns
                .iter()
                .position(|c| &c.name == key)
                .is_some_and(|pos| aligned[pos] == session.original_row[pos])
        });
    if is_session_row {
        session.original_row.clone()
    } else {
        aligned
    }
}

/// Plain character types that free-text rewrites may touch. Keys, numerics,
/// dates, and structured text (JSON/XML) are never touched: rewriting inside
/// them is more likely to corrupt a value than to do what the user meant.
pub fn is_plain_text_type(data_type: &str) -> bool {
    let base = data_type
        .split('(')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "char"
            | "varchar"
            | "character"
            | "character varying"
            | "char varying"
            | "nchar"
            | "nvarchar"
            | "national char"
            | "national character"
            | "national character varying"
            | "text"
            | "ntext"
            | "tinytext"
            | "mediumtext"
            | "longtext"
            | "citext"
            | "clob"
            | "nclob"
            | "varchar2"
            | "nvarchar2"
            | "long varchar"
            | "string"
            | "bpchar"
            | "sysname"
            // SQLite columns declared without a type store whatever they are
            // given; string values in them are plain text.
            | ""
    )
}

/// The replacement value for one matched cell: every case-insensitive
/// occurrence of `needle` in a string cell swapped for `replacement` (the
/// find bar matches case-insensitively, so replace does too). Returns `None`
/// when the cell is not a string or holds no occurrence — NULL cells in
/// particular never match, even though the grid renders a searchable "NULL"
/// placeholder for them.
pub fn replaced_cell(current: &CellValue, needle: &str, replacement: &str) -> Option<CellValue> {
    let CellValue::String(text) = current else {
        return None;
    };
    let replaced = replace_case_insensitive(text, needle, replacement)?;
    if replaced == *text {
        return None;
    }
    Some(CellValue::String(replaced))
}

/// Case-insensitive replace-all on the raw text. Works char-by-char so
/// Unicode case folding never desynchronizes the byte offsets; `None` when
/// the needle is empty or absent.
fn replace_case_insensitive(text: &str, needle: &str, replacement: &str) -> Option<String> {
    if needle.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut found = false;
    while let Some(ch) = rest.chars().next() {
        if let Some(match_len) = prefix_match_len(rest, needle) {
            out.push_str(replacement);
            rest = &rest[match_len..];
            found = true;
        } else {
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    found.then_some(out)
}

/// Byte length of `needle` matched at the start of `text`,
/// case-insensitively, if it matches at all.
fn prefix_match_len(text: &str, needle: &str) -> Option<usize> {
    let mut len = 0;
    let mut chars = text.chars();
    for expected in needle.chars() {
        let actual = chars.next()?;
        if actual != expected && !actual.to_lowercase().eq(expected.to_lowercase()) {
            return None;
        }
        len += actual.len_utf8();
    }
    Some(len)
}

/// Map matched cells onto per-row value edits, enforcing the editability
/// rules. `matches` are `(row_index_into_rows, column_index)` pairs in the
/// caller's coordinate space; a match is skipped when its row or column is
/// out of range, the column is not replaceable, or the cell holds no
/// occurrence of `needle`. `rows` should carry each cell's *effective* value
/// (staged edit overlaid on the original) so a replace composes with earlier
/// edits. Edits come back in match order, keyed by table column name.
pub fn plan_replacements(
    session: &RowEditSession,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    matches: &[(usize, usize)],
    needle: &str,
    replacement: &str,
) -> Vec<ReplaceEdit> {
    let mut out = Vec::new();
    for &(row, col) in matches {
        if col >= columns.len() {
            continue;
        }
        let Some(table_column) = table_column_for_result(session, col) else {
            continue;
        };
        if !is_replaceable(table_column) {
            continue;
        }
        let column = table_column.name.clone();
        let Some(cell) = rows.get(row).and_then(|r| r.get(col)) else {
            continue;
        };
        if let Some(value) = replaced_cell(cell, needle, replacement) {
            out.push(ReplaceEdit { row, column, value });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_editing::{RowEditColumn, TableTarget};

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

    fn session() -> RowEditSession {
        let mut id = column("id", "int");
        id.is_primary_key = true;
        id.is_identity = true;
        let mut computed = column("upper_name", "nvarchar");
        computed.is_computed = true;
        let mut status = column("status", "varchar");
        status.enum_values = vec!["open".into(), "closed".into()];
        RowEditSession {
            target: TableTarget {
                schema: "dbo".to_string(),
                table: "Items".to_string(),
            },
            key_columns: vec!["id".to_string()],
            rowversion_column: None,
            // Provenance: every column is projected at its own position,
            // matching `result_columns()`.
            columns: vec![
                id,
                column("name", "nvarchar"),
                column("notes", "varchar(max)"),
                column("amount", "decimal"),
                computed,
                status,
            ]
            .into_iter()
            .enumerate()
            .map(|(ordinal, mut column)| {
                column.result_ordinal = Some(ordinal);
                column
            })
            .collect(),
            original_row: Vec::new(),
            foreign_keys: Vec::new(),
            fk_display: Vec::new(),
            check_constraints: Vec::new(),
            incoming_foreign_keys: Vec::new(),
        }
    }

    fn result_columns() -> Vec<ResultColumn> {
        ["id", "name", "notes", "amount", "upper_name", "status"]
            .into_iter()
            .map(|name| ResultColumn {
                name: name.to_string(),
                data_type: "x".to_string(),
                nullable: true,
            })
            .collect()
    }

    #[test]
    fn column_replaceable_requires_editable_textual_non_enum() {
        let session = session();
        assert!(column_replaceable(&session, "name"));
        assert!(column_replaceable(&session, "notes"));
        assert!(!column_replaceable(&session, "id"), "identity key");
        assert!(!column_replaceable(&session, "amount"), "numeric");
        assert!(!column_replaceable(&session, "upper_name"), "computed");
        assert!(!column_replaceable(&session, "status"), "enum values");
        assert!(!column_replaceable(&session, "missing"), "unknown column");
    }

    #[test]
    fn textual_type_spellings() {
        assert!(is_plain_text_type("nvarchar"));
        assert!(is_plain_text_type("varchar(50)"));
        assert!(is_plain_text_type("character varying"));
        assert!(is_plain_text_type("TEXT"));
        assert!(is_plain_text_type("citext"));
        assert!(!is_plain_text_type("int"));
        assert!(!is_plain_text_type("json"));
        assert!(!is_plain_text_type("xml"));
        assert!(!is_plain_text_type("uuid"));
        assert!(!is_plain_text_type("datetime2"));
        assert!(!is_plain_text_type("jsonb"));
        assert!(!is_plain_text_type("enum"));
        assert!(!is_plain_text_type("set"));
        assert!(is_plain_text_type("bpchar"));
        assert!(is_plain_text_type(""), "untyped SQLite column");
    }

    #[test]
    fn plain_text_column_excludes_enum_constrained_text() {
        let mut status = column("status", "varchar");
        assert!(is_plain_text_column(&status));
        status.enum_values = vec!["open".into()];
        assert!(!is_plain_text_column(&status));
        assert!(!is_plain_text_column(&column("doc", "json")));
        assert!(!is_plain_text_column(&column("flags", "set")));
    }

    #[test]
    fn replaced_cell_is_case_insensitive_and_replaces_every_occurrence() {
        let cell = CellValue::String("Alpha alpha ALPHA".into());
        assert_eq!(
            replaced_cell(&cell, "alpha", "beta"),
            Some(CellValue::String("beta beta beta".into()))
        );
    }

    #[test]
    fn replaced_cell_handles_unicode_case() {
        let cell = CellValue::String("Café au CAFÉ".into());
        assert_eq!(
            replaced_cell(&cell, "café", "x"),
            Some(CellValue::String("x au x".into()))
        );
    }

    #[test]
    fn replaced_cell_skips_null_non_string_no_match_and_empty_needle() {
        assert_eq!(replaced_cell(&CellValue::Null, "null", "x"), None);
        assert_eq!(replaced_cell(&CellValue::I64(42), "42", "x"), None);
        assert_eq!(
            replaced_cell(&CellValue::String("hello".into()), "bye", "x"),
            None
        );
        assert_eq!(
            replaced_cell(&CellValue::String("hello".into()), "", "x"),
            None
        );
    }

    #[test]
    fn replaced_cell_none_when_replacement_changes_nothing() {
        // Literally identical text is a no-op (avoids empty staged edits);
        // a case-only difference still stages.
        let cell = CellValue::String("alpha".into());
        assert_eq!(replaced_cell(&cell, "ALPHA", "alpha"), None);
        let loud = CellValue::String("ALPHA".into());
        assert_eq!(
            replaced_cell(&loud, "alpha", "alpha"),
            Some(CellValue::String("alpha".into()))
        );
    }

    #[test]
    fn plan_replacements_maps_only_writable_matches() {
        let session = session();
        let columns = result_columns();
        let rows = vec![
            vec![
                CellValue::I64(1),
                CellValue::String("foo".into()),
                CellValue::Null,
                CellValue::String("9.5".into()),
                CellValue::String("FOO".into()),
                CellValue::String("foo".into()),
            ],
            vec![
                CellValue::I64(2),
                CellValue::String("bar foo bar".into()),
                CellValue::String("foo".into()),
                CellValue::Null,
                CellValue::Null,
                CellValue::String("open".into()),
            ],
        ];
        // Match every cell of both rows; only the text, non-key, non-computed,
        // non-enum columns holding "foo" should produce edits.
        let matches: Vec<(usize, usize)> =
            (0..2).flat_map(|r| (0..6).map(move |c| (r, c))).collect();
        let edits = plan_replacements(&session, &columns, &rows, &matches, "foo", "baz");
        assert_eq!(
            edits,
            vec![
                ReplaceEdit {
                    row: 0,
                    column: "name".to_string(),
                    value: CellValue::String("baz".into()),
                },
                ReplaceEdit {
                    row: 1,
                    column: "name".to_string(),
                    value: CellValue::String("bar baz bar".into()),
                },
                ReplaceEdit {
                    row: 1,
                    column: "notes".to_string(),
                    value: CellValue::String("baz".into()),
                },
            ]
        );
    }

    #[test]
    fn plan_replacements_resolves_aliased_columns_through_provenance() {
        // SELECT id, name AS display_name — the result column is aliased; the
        // staged edit must target the table column `name`.
        let mut session = session();
        session.columns[0].result_ordinal = Some(0);
        session.columns[1].result_ordinal = Some(1);
        let columns: Vec<ResultColumn> = ["id", "display_name"]
            .into_iter()
            .map(|name| ResultColumn {
                name: name.to_string(),
                data_type: "x".to_string(),
                nullable: true,
            })
            .collect();
        let rows = vec![vec![CellValue::I64(1), CellValue::String("foo".into())]];
        let edits = plan_replacements(&session, &columns, &rows, &[(0, 1)], "foo", "bar");
        assert_eq!(
            edits,
            vec![ReplaceEdit {
                row: 0,
                column: "name".to_string(),
                value: CellValue::String("bar".into()),
            }]
        );
    }

    /// `SELECT id, left(notes, 3) AS notes FROM Items`: the result column
    /// `notes` is an expression that merely shares the table column's name.
    /// Provenance leaves `notes` unprojected, so the match must not be
    /// written back — a name fallback would store the truncated text in the
    /// real column.
    fn same_name_expression_session() -> (RowEditSession, Vec<ResultColumn>) {
        let mut session = session();
        for column in &mut session.columns {
            column.result_ordinal = None;
        }
        session.columns[0].result_ordinal = Some(0);
        let columns: Vec<ResultColumn> = ["id", "notes"]
            .into_iter()
            .map(|name| ResultColumn {
                name: name.to_string(),
                data_type: "x".to_string(),
                nullable: true,
            })
            .collect();
        (session, columns)
    }

    #[test]
    fn plan_replacements_never_writes_a_same_name_expression_column() {
        let (session, columns) = same_name_expression_session();
        let rows = vec![vec![CellValue::I64(1), CellValue::String("foo".into())]];
        let edits = plan_replacements(&session, &columns, &rows, &[(0, 1)], "foo", "bar");
        assert!(
            edits.is_empty(),
            "unprojected column was written: {edits:?}"
        );
    }

    #[test]
    fn session_aligned_row_ignores_a_same_name_expression_column() {
        let (session, _columns) = same_name_expression_session();
        let aligned = session_aligned_row(
            &session,
            &[CellValue::I64(1), CellValue::String("tru".into())],
        );
        let notes = session
            .columns
            .iter()
            .position(|c| c.name == "notes")
            .unwrap();
        assert_eq!(aligned[0], CellValue::I64(1));
        assert_eq!(
            aligned[notes],
            CellValue::Null,
            "the expression's text must not become the column's original"
        );
    }

    #[test]
    fn table_column_for_result_uses_provenance_only() {
        let (session, _columns) = same_name_expression_session();
        assert_eq!(
            table_column_for_result(&session, 0).map(|c| c.name.as_str()),
            Some("id")
        );
        assert!(table_column_for_result(&session, 1).is_none());
    }

    #[test]
    fn session_aligned_row_maps_projection_and_nulls_unprojected() {
        let mut session = session();
        // Result projects (name, id) in that order; other columns absent.
        session.columns[0].result_ordinal = Some(1);
        session.columns[1].result_ordinal = Some(0);
        session.original_row = vec![
            CellValue::I64(1),
            CellValue::String("one".into()),
            CellValue::String("notes-1".into()),
            CellValue::Null,
            CellValue::Null,
            CellValue::Null,
        ];
        let other = session_aligned_row(
            &session,
            &[CellValue::String("two".into()), CellValue::I64(2)],
        );
        assert_eq!(other[0], CellValue::I64(2));
        assert_eq!(other[1], CellValue::String("two".into()));
        assert_eq!(other[2], CellValue::Null, "unprojected column is unknown");
        // The session's own row keeps the fetched values, unprojected ones too.
        let own = session_aligned_row(
            &session,
            &[CellValue::String("one".into()), CellValue::I64(1)],
        );
        assert_eq!(own, session.original_row);
    }

    #[test]
    fn plan_replacements_ignores_out_of_range_coordinates() {
        let session = session();
        let columns = result_columns();
        let rows = vec![vec![
            CellValue::I64(1),
            CellValue::String("foo".into()),
            CellValue::Null,
            CellValue::Null,
            CellValue::Null,
            CellValue::Null,
        ]];
        let edits = plan_replacements(
            &session,
            &columns,
            &rows,
            &[(5, 1), (0, 99), (0, 1)],
            "foo",
            "x",
        );
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].column, "name");
    }
}
