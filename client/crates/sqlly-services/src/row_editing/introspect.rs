//! Column introspection and provenance analysis: parsing the engine's own
//! description of an executed statement, deciding whether a result maps onto
//! exactly one editable base table, declared-length arithmetic, and grouping
//! flat FK metadata rows into constraint specs. All pure functions, unit
//! tested without a server.

use sqlly_core::{CellValue, DisplayMessage, MaterializedResultSet, ResultColumn};

use super::types::{
    CheckConstraintSpec, ForeignKeySpec, IncomingForeignKey, RowEditColumn, TableTarget,
};

/// One visible column of `sp_describe_first_result_set … @browse_information_mode = 1`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DescribeColumn {
    pub name: String,
    pub is_hidden: bool,
    pub source_schema: Option<String>,
    pub source_table: Option<String>,
    pub source_column: Option<String>,
}

/// Provenance of a result set that maps onto exactly one base table.
#[derive(Debug, Clone, PartialEq)]
pub struct Provenance {
    pub target: TableTarget,
    /// `(source column name, ordinal in the projected result)` pairs.
    pub source_ordinals: Vec<(String, usize)>,
}

/// Parses the result set returned by `sp_describe_first_result_set` into
/// [`DescribeColumn`]s, looking fields up by column name so this is robust to
/// the procedure's column ordering.
pub fn parse_describe_result(set: &MaterializedResultSet) -> Vec<DescribeColumn> {
    let index_of = |wanted: &str| {
        set.columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(wanted))
    };
    let name_at = index_of("name");
    let hidden_at = index_of("is_hidden");
    let schema_at = index_of("source_schema");
    let table_at = index_of("source_table");
    let column_at = index_of("source_column");

    let cell_string = |row: &[CellValue], at: Option<usize>| -> Option<String> {
        match at.and_then(|i| row.get(i)) {
            Some(CellValue::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        }
    };
    let cell_bool = |row: &[CellValue], at: Option<usize>| -> bool {
        matches!(
            at.and_then(|i| row.get(i)),
            Some(CellValue::Bool(true)) | Some(CellValue::I64(1))
        )
    };

    set.rows
        .iter()
        .map(|row| DescribeColumn {
            name: cell_string(row, name_at).unwrap_or_default(),
            is_hidden: cell_bool(row, hidden_at),
            source_schema: cell_string(row, schema_at),
            source_table: cell_string(row, table_at),
            source_column: cell_string(row, column_at),
        })
        .collect()
}

/// Decides whether described columns map onto exactly one editable base
/// table, and builds the source-column → result-ordinal mapping.
///
/// Rules (v1):
/// - visible described columns must match the projected column count (the
///   description must be talking about the grid we clicked in);
/// - at least one column must carry source provenance;
/// - every sourced column must come from the same schema+table (joins are
///   rejected); columns without provenance (expressions) are tolerated and
///   simply end up read-only.
pub fn analyze_provenance(
    described: &[DescribeColumn],
    projected: &[ResultColumn],
) -> Result<Provenance, DisplayMessage> {
    let visible: Vec<&DescribeColumn> = described.iter().filter(|c| !c.is_hidden).collect();
    if visible.is_empty() {
        return Err(crate::row_editing::not_editable::result_shape_unknown());
    }
    if visible.len() != projected.len() {
        return Err(crate::row_editing::not_editable::column_count_mismatch(
            visible.len(),
            projected.len(),
        ));
    }

    let mut target: Option<TableTarget> = None;
    let mut source_ordinals: Vec<(String, usize)> = Vec::new();
    for (ordinal, column) in visible.iter().enumerate() {
        let (Some(schema), Some(table), Some(source_column)) = (
            column.source_schema.as_ref(),
            column.source_table.as_ref(),
            column.source_column.as_ref(),
        ) else {
            continue; // expression column: read-only, does not block editing
        };
        let this = TableTarget {
            schema: schema.clone(),
            table: table.clone(),
        };
        match &target {
            None => target = Some(this),
            Some(existing) if *existing == this => {}
            Some(existing) => {
                return Err(crate::row_editing::not_editable::multiple_tables(
                    &existing.quoted(),
                    &this.quoted(),
                ));
            }
        }
        // First occurrence wins for duplicated projections of one column.
        if !source_ordinals
            .iter()
            .any(|(name, _)| name == source_column)
        {
            source_ordinals.push((source_column.clone(), ordinal));
        }
    }

    let Some(target) = target else {
        return Err(crate::row_editing::not_editable::no_source_column());
    };
    Ok(Provenance {
        target,
        source_ordinals,
    })
}

/// Character length for a declared type from `sys.columns.max_length` bytes.
/// `None` for `max` types (-1) and types without a length.
pub fn declared_char_length(type_name: &str, max_length_bytes: i64) -> Option<i64> {
    if max_length_bytes < 0 {
        return None; // varchar(max) / nvarchar(max) / varbinary(max)
    }
    match type_name.to_ascii_lowercase().as_str() {
        "nvarchar" | "nchar" => Some(max_length_bytes / 2),
        "varchar" | "char" | "binary" | "varbinary" => Some(max_length_bytes),
        _ => None,
    }
}

/// One flat row of the outgoing-FK metadata query (before grouping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkColumnRow {
    pub constraint_name: String,
    pub ref_schema: String,
    pub ref_table: String,
    pub parent_column: String,
    pub referenced_column: String,
    pub display_column: Option<String>,
}

/// Groups flat FK column-pair rows (constraint_name, ref_schema, ref_table,
/// parent_column, ref_column, ordinal, display_column) into [`ForeignKeySpec`]s,
/// one per constraint, with column pairs in key order.
pub fn group_foreign_keys(rows: &[FkColumnRow]) -> Vec<ForeignKeySpec> {
    let mut specs: Vec<ForeignKeySpec> = Vec::new();
    for row in rows {
        match specs
            .iter_mut()
            .find(|spec| spec.constraint_name == row.constraint_name)
        {
            Some(spec) => {
                spec.column_pairs
                    .push((row.parent_column.clone(), row.referenced_column.clone()));
                if spec.display_column.is_none() {
                    spec.display_column = row.display_column.clone();
                }
            }
            None => specs.push(ForeignKeySpec {
                constraint_name: row.constraint_name.clone(),
                referenced: TableTarget {
                    schema: row.ref_schema.clone(),
                    table: row.ref_table.clone(),
                },
                column_pairs: vec![(row.parent_column.clone(), row.referenced_column.clone())],
                display_column: row.display_column.clone(),
            }),
        }
    }
    specs
}

/// One flat row of the incoming-FK metadata query (before grouping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingFkColumnRow {
    pub constraint_name: String,
    pub child_schema: String,
    pub child_table: String,
    pub child_column: String,
    pub this_column: String,
}

/// Groups flat incoming-FK column rows into [`IncomingForeignKey`]s, one per
/// constraint, with column pairs in key order.
pub fn group_incoming_foreign_keys(rows: &[IncomingFkColumnRow]) -> Vec<IncomingForeignKey> {
    let mut specs: Vec<IncomingForeignKey> = Vec::new();
    for row in rows {
        match specs
            .iter_mut()
            .find(|spec| spec.constraint_name == row.constraint_name)
        {
            Some(spec) => spec
                .column_pairs
                .push((row.child_column.clone(), row.this_column.clone())),
            None => specs.push(IncomingForeignKey {
                constraint_name: row.constraint_name.clone(),
                child: TableTarget {
                    schema: row.child_schema.clone(),
                    table: row.child_table.clone(),
                },
                column_pairs: vec![(row.child_column.clone(), row.this_column.clone())],
            }),
        }
    }
    specs
}

/// Authoritative catalog metadata for one base table. Shared vocabulary
/// between the T-SQL loader in `service` and the per-engine loaders in
/// `row_editing_pg` / `row_editing_mysql` / `row_editing_sqlite`.
pub(crate) struct LoadedTable {
    pub(crate) columns: Vec<RowEditColumn>,
    pub(crate) key_columns: Vec<String>,
    pub(crate) foreign_keys: Vec<ForeignKeySpec>,
    pub(crate) check_constraints: Vec<CheckConstraintSpec>,
    pub(crate) incoming_foreign_keys: Vec<IncomingForeignKey>,
}

/// Parses the quoted value list from a MySQL `ENUM(...)` or `SET(...)` `COLUMN_TYPE`.
/// Handles `''` and `\'` escapes; returns an empty vec when the type is not an enum/set.
pub fn parse_mysql_enum_list(column_type: &str) -> Vec<String> {
    let trimmed = column_type.trim();
    let lower = trimmed.to_ascii_lowercase();
    let prefix_len = if lower.starts_with("enum(") {
        5
    } else if lower.starts_with("set(") {
        4
    } else {
        return Vec::new();
    };
    let inner = &trimmed[prefix_len..];
    let Some(close) = matching_delim(inner, '(', ')') else {
        return Vec::new();
    };
    parse_sql_string_csv(&inner[..close]).unwrap_or_default()
}

/// Allowed values from a single-column CHECK that is an `IN` list, SQL Server
/// `=`/`OR` chain, or PostgreSQL `= ANY (ARRAY[...])`. `None` when the
/// constraint names a different column, names more than one column, or is not
/// one of those shapes.
pub fn enum_values_from_check(column: &str, definition: &str) -> Option<Vec<String>> {
    let body = strip_check_wrapper(definition);
    try_in_form(column, body)
        .or_else(|| try_or_equals_form(column, body))
        .or_else(|| try_any_array_form(column, body))
}

/// Fill empty `enum_values` from CHECK constraints that enumerate a single column.
pub fn fill_enum_values_from_checks(columns: &mut [RowEditColumn], checks: &[CheckConstraintSpec]) {
    for column in columns.iter_mut() {
        if !column.enum_values.is_empty() {
            continue;
        }
        for check in checks {
            if let Some(values) = enum_values_from_check(&column.name, &check.definition) {
                column.enum_values = values;
                break;
            }
        }
    }
}

fn strip_check_wrapper(s: &str) -> &str {
    let mut t = s.trim();
    let lower = t.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("check") {
        t = t[t.len() - rest.len()..].trim();
    }
    loop {
        t = t.trim();
        if !t.starts_with('(') || !t.ends_with(')') {
            break;
        }
        let inner = &t[1..];
        match matching_delim(inner, '(', ')') {
            Some(close) if close + 1 == inner.len() => {
                t = t[1..t.len() - 1].trim();
            }
            _ => break,
        }
    }
    t
}

fn matching_delim(s: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 1;
    let mut in_str = false;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if in_str {
            if c == '\\' {
                chars.next();
                continue;
            }
            if c == '\'' {
                if chars.peek().is_some_and(|(_, n)| *n == '\'') {
                    chars.next();
                    continue;
                }
                in_str = false;
            }
            continue;
        }
        if c == '\'' {
            in_str = true;
            continue;
        }
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

fn skip_ws(s: &str) -> &str {
    s.trim_start()
}

fn skip_pg_cast(s: &str) -> &str {
    let t = skip_ws(s);
    let Some(rest) = t.strip_prefix("::") else {
        return t;
    };
    let rest = skip_ws(rest);
    let end = rest
        .char_indices()
        .find(|(_, c)| {
            !c.is_ascii_alphanumeric() && *c != '_' && *c != ' ' && *c != '[' && *c != ']'
        })
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    &rest[end..]
}

fn parse_simple_ident(s: &str) -> Option<(String, &str)> {
    let s = skip_ws(s);
    if let Some(inner) = s.strip_prefix('[') {
        let end = inner.find(']')?;
        Some((inner[..end].replace("]]", "]"), &inner[end + 1..]))
    } else if let Some(inner) = s.strip_prefix('`') {
        let end = inner.find('`')?;
        Some((inner[..end].to_string(), &inner[end + 1..]))
    } else if let Some(inner) = s.strip_prefix('"') {
        let end = inner.find('"')?;
        Some((inner[..end].to_string(), &inner[end + 1..]))
    } else {
        let end = s
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(s.len());
        if end == 0 {
            return None;
        }
        Some((s[..end].to_string(), &s[end..]))
    }
}

fn parse_column_ref(s: &str) -> Option<(String, &str)> {
    let s = skip_ws(s);
    if let Some(inner) = s.strip_prefix('(') {
        let close = matching_delim(inner, '(', ')')?;
        let (ident, leftover) = parse_column_ref(&inner[..close])?;
        if !skip_ws(leftover).is_empty() {
            return None;
        }
        let rest = skip_pg_cast(skip_ws(&inner[close + 1..]));
        return Some((ident, rest));
    }
    let (ident, rest) = parse_simple_ident(s)?;
    Some((ident, skip_pg_cast(rest)))
}

fn parse_sql_string(s: &str) -> Option<(String, &str)> {
    let s = skip_ws(s);
    let rest = s.strip_prefix('\'')?;
    let mut out = String::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            if let Some((_, next)) = chars.next() {
                out.push(next);
            }
            continue;
        }
        if c == '\'' {
            if chars.peek().is_some_and(|(_, n)| *n == '\'') {
                chars.next();
                out.push('\'');
                continue;
            }
            let after = &rest[i + 1..];
            return Some((out, skip_pg_cast(after)));
        }
        out.push(c);
    }
    None
}

fn parse_sql_string_csv(s: &str) -> Option<Vec<String>> {
    let mut rest = skip_ws(s);
    if rest.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    loop {
        let (value, next) = parse_sql_string(rest)?;
        out.push(value);
        rest = skip_ws(next);
        if rest.is_empty() {
            return Some(out);
        }
        rest = skip_ws(rest.strip_prefix(',')?);
        if rest.is_empty() {
            return None;
        }
    }
}

fn is_keyword_at(s: &str, word: &str) -> bool {
    let s = skip_ws(s);
    if s.len() < word.len() || !s[..word.len()].eq_ignore_ascii_case(word) {
        return false;
    }
    !s[word.len()..]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn take_keyword<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let s = skip_ws(s);
    if !is_keyword_at(s, word) {
        return None;
    }
    Some(&s[word.len()..])
}

fn skip_trailing_noise(s: &str) -> &str {
    let mut t = skip_ws(s);
    loop {
        let after_cast = skip_ws(skip_pg_cast(t));
        if let Some(stripped) = after_cast.strip_prefix(')') {
            t = stripped;
            continue;
        }
        if after_cast == t {
            return skip_ws(t);
        }
        t = after_cast;
    }
}

fn try_in_form(column: &str, body: &str) -> Option<Vec<String>> {
    let (ident, rest) = parse_column_ref(body)?;
    if !ident.eq_ignore_ascii_case(column) {
        return None;
    }
    let rest = take_keyword(rest, "in")?;
    let rest = skip_ws(rest);
    let inner = rest.strip_prefix('(')?;
    let close = matching_delim(inner, '(', ')')?;
    let rest_after = skip_ws(&inner[close + 1..]);
    if !rest_after.is_empty() {
        return None;
    }
    parse_sql_string_csv(&inner[..close])
}

fn try_or_equals_form(column: &str, body: &str) -> Option<Vec<String>> {
    let mut rest = body;
    let mut values = Vec::new();
    let mut first = true;
    loop {
        if !first {
            rest = take_keyword(rest, "or")?;
        }
        first = false;
        let (ident, after_ident) = parse_column_ref(rest)?;
        if !ident.eq_ignore_ascii_case(column) {
            return None;
        }
        let after_eq = skip_ws(after_ident).strip_prefix('=')?;
        let (value, after_value) = parse_sql_string(after_eq)?;
        values.push(value);
        rest = after_value;
        if skip_ws(rest).is_empty() {
            break;
        }
    }
    (values.len() >= 2).then_some(values)
}

fn try_any_array_form(column: &str, body: &str) -> Option<Vec<String>> {
    let (ident, rest) = parse_column_ref(body)?;
    if !ident.eq_ignore_ascii_case(column) {
        return None;
    }
    let rest = skip_ws(rest).strip_prefix('=')?;
    let rest = take_keyword(rest, "any")?;
    let lower = rest.to_ascii_lowercase();
    let array_at = lower.find("array[")?;
    let after_bracket = &rest[array_at + "array[".len()..];
    let close = matching_delim(after_bracket, '[', ']')?;
    let rest_after = skip_trailing_noise(&after_bracket[close + 1..]);
    if !rest_after.is_empty() {
        return None;
    }
    parse_sql_string_csv(&after_bracket[..close])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: "int".to_string(),
            nullable: true,
        }
    }

    fn described(name: &str, hidden: bool, source: Option<(&str, &str, &str)>) -> DescribeColumn {
        DescribeColumn {
            name: name.to_string(),
            is_hidden: hidden,
            source_schema: source.map(|(s, _, _)| s.to_string()),
            source_table: source.map(|(_, t, _)| t.to_string()),
            source_column: source.map(|(_, _, c)| c.to_string()),
        }
    }

    #[test]
    fn provenance_single_table_maps_ordinals() {
        let described = vec![
            described("id", false, Some(("dbo", "Orders", "OrderId"))),
            described("name", false, Some(("dbo", "Orders", "Name"))),
        ];
        let projected = vec![column("id"), column("name")];
        let provenance = analyze_provenance(&described, &projected).expect("editable");
        assert_eq!(
            provenance.target,
            TableTarget {
                schema: "dbo".into(),
                table: "Orders".into()
            }
        );
        assert_eq!(
            provenance.source_ordinals,
            vec![("OrderId".to_string(), 0), ("Name".to_string(), 1)]
        );
    }

    #[test]
    fn provenance_rejects_joins() {
        let described = vec![
            described("a", false, Some(("dbo", "Orders", "Id"))),
            described("b", false, Some(("dbo", "Customers", "Id"))),
        ];
        let projected = vec![column("a"), column("b")];
        let err = analyze_provenance(&described, &projected).unwrap_err();
        assert!(err.contains("more than one table"), "{err}");
    }

    #[test]
    fn provenance_rejects_pure_expression_results() {
        let described = vec![described("calc", false, None)];
        let projected = vec![column("calc")];
        let err = analyze_provenance(&described, &projected).unwrap_err();
        assert!(err.contains("read-only"), "{err}");
    }

    #[test]
    fn provenance_tolerates_expression_columns_alongside_sourced_ones() {
        let described = vec![
            described("id", false, Some(("dbo", "Orders", "Id"))),
            described("calc", false, None),
        ];
        let projected = vec![column("id"), column("calc")];
        let provenance = analyze_provenance(&described, &projected).expect("editable");
        assert_eq!(provenance.source_ordinals, vec![("Id".to_string(), 0)]);
    }

    #[test]
    fn provenance_ignores_hidden_browse_columns_for_shape_match() {
        let described = vec![
            described("id", false, Some(("dbo", "Orders", "Id"))),
            described("Id", true, Some(("dbo", "Orders", "Id"))),
        ];
        let projected = vec![column("id")];
        let provenance = analyze_provenance(&described, &projected).expect("editable");
        assert_eq!(provenance.source_ordinals.len(), 1);
    }

    #[test]
    fn provenance_rejects_shape_mismatch() {
        let described = vec![described("id", false, Some(("dbo", "Orders", "Id")))];
        let projected = vec![column("id"), column("extra")];
        let err = analyze_provenance(&described, &projected).unwrap_err();
        assert!(err.contains("provenance cannot be trusted"), "{err}");
    }

    #[test]
    fn provenance_first_occurrence_wins_for_duplicate_projection() {
        let described = vec![
            described("id", false, Some(("dbo", "Orders", "Id"))),
            described("id2", false, Some(("dbo", "Orders", "Id"))),
        ];
        let projected = vec![column("id"), column("id2")];
        let provenance = analyze_provenance(&described, &projected).expect("editable");
        assert_eq!(provenance.source_ordinals, vec![("Id".to_string(), 0)]);
    }

    #[test]
    fn provenance_rejects_empty_description() {
        let err = analyze_provenance(&[], &[column("id")]).unwrap_err();
        assert!(err.contains("could not be determined"), "{err}");
    }

    #[test]
    fn declared_char_length_halves_nvarchar_and_passes_varchar() {
        assert_eq!(declared_char_length("nvarchar", 100), Some(50));
        assert_eq!(declared_char_length("nchar", 20), Some(10));
        assert_eq!(declared_char_length("varchar", 100), Some(100));
        assert_eq!(declared_char_length("char", 8), Some(8));
        assert_eq!(declared_char_length("varbinary", 16), Some(16));
        assert_eq!(declared_char_length("binary", 16), Some(16));
    }

    #[test]
    fn declared_char_length_none_for_max_and_unlengthed_types() {
        assert_eq!(declared_char_length("nvarchar", -1), None);
        assert_eq!(declared_char_length("varchar", -1), None);
        assert_eq!(declared_char_length("int", 4), None);
        assert_eq!(declared_char_length("datetime2", 8), None);
    }

    #[test]
    fn declared_char_length_is_case_insensitive() {
        assert_eq!(declared_char_length("NVARCHAR", 100), Some(50));
        assert_eq!(declared_char_length("VarChar", 30), Some(30));
    }

    #[test]
    fn declared_char_length_zero_stays_zero() {
        assert_eq!(declared_char_length("varchar", 0), Some(0));
        assert_eq!(declared_char_length("nvarchar", 0), Some(0));
    }

    #[test]
    fn parse_describe_result_reads_fields_by_name() {
        let set = MaterializedResultSet {
            columns: vec![
                column("is_hidden"),
                column("name"),
                column("source_schema"),
                column("source_table"),
                column("source_column"),
            ],
            rows: vec![vec![
                CellValue::Bool(false),
                CellValue::String("id".into()),
                CellValue::String("dbo".into()),
                CellValue::String("Orders".into()),
                CellValue::String("Id".into()),
            ]],
        };
        let described = parse_describe_result(&set);
        assert_eq!(
            described,
            vec![DescribeColumn {
                name: "id".into(),
                is_hidden: false,
                source_schema: Some("dbo".into()),
                source_table: Some("Orders".into()),
                source_column: Some("Id".into()),
            }]
        );
    }

    #[test]
    fn parse_describe_result_treats_empty_and_null_source_as_none() {
        let set = MaterializedResultSet {
            columns: vec![
                column("name"),
                column("is_hidden"),
                column("source_schema"),
                column("source_table"),
                column("source_column"),
            ],
            rows: vec![vec![
                CellValue::String("calc".into()),
                // is_hidden as an integer flag, the other TDS shape.
                CellValue::I64(1),
                CellValue::String(String::new()),
                CellValue::Null,
                CellValue::Null,
            ]],
        };
        let described = parse_describe_result(&set);
        assert_eq!(
            described,
            vec![DescribeColumn {
                name: "calc".into(),
                is_hidden: true,
                source_schema: None,
                source_table: None,
                source_column: None,
            }]
        );
    }

    #[test]
    fn parse_describe_result_tolerates_missing_columns() {
        let set = MaterializedResultSet {
            columns: vec![column("name")],
            rows: vec![vec![CellValue::String("only".into())]],
        };
        let described = parse_describe_result(&set);
        assert_eq!(
            described,
            vec![DescribeColumn {
                name: "only".into(),
                is_hidden: false,
                source_schema: None,
                source_table: None,
                source_column: None,
            }]
        );
    }

    fn fk_row(
        constraint: &str,
        parent: &str,
        referenced: &str,
        display: Option<&str>,
    ) -> FkColumnRow {
        FkColumnRow {
            constraint_name: constraint.to_string(),
            ref_schema: "dbo".to_string(),
            ref_table: "Customer".to_string(),
            parent_column: parent.to_string(),
            referenced_column: referenced.to_string(),
            display_column: display.map(|d| d.to_string()),
        }
    }

    #[test]
    fn group_foreign_keys_merges_composite_pairs_in_order() {
        let rows = vec![
            fk_row("FK_A", "CustomerId", "Id", Some("Name")),
            fk_row("FK_Composite", "RegionId", "RegionId", None),
            fk_row("FK_Composite", "SubId", "SubId", None),
        ];
        let specs = group_foreign_keys(&rows);
        assert_eq!(specs.len(), 2);
        let a = &specs[0];
        assert_eq!(a.constraint_name, "FK_A");
        assert_eq!(a.display_column.as_deref(), Some("Name"));
        assert_eq!(a.column_pairs, vec![("CustomerId".into(), "Id".into())]);
        let composite = &specs[1];
        assert_eq!(
            composite.column_pairs,
            vec![
                ("RegionId".to_string(), "RegionId".to_string()),
                ("SubId".to_string(), "SubId".to_string())
            ]
        );
        assert_eq!(composite.local_columns(), vec!["RegionId", "SubId"]);
        assert_eq!(composite.referenced_columns(), vec!["RegionId", "SubId"]);
    }

    #[test]
    fn incoming_fk_grouping_merges_pairs() {
        let rows = vec![
            IncomingFkColumnRow {
                constraint_name: "FK_Order_Customer".into(),
                child_schema: "dbo".into(),
                child_table: "Orders".into(),
                child_column: "CustomerId".into(),
                this_column: "Id".into(),
            },
            IncomingFkColumnRow {
                constraint_name: "FK_Invoice_Customer".into(),
                child_schema: "sales".into(),
                child_table: "Invoice".into(),
                child_column: "CustId".into(),
                this_column: "Id".into(),
            },
        ];
        let specs = group_incoming_foreign_keys(&rows);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].child.table, "Orders");
        assert_eq!(
            specs[0].column_pairs,
            vec![("CustomerId".to_string(), "Id".to_string())]
        );
    }

    #[test]
    fn mysql_enum_list_parses_escapes() {
        assert_eq!(
            parse_mysql_enum_list("enum('a','b''c','d\\'e')"),
            vec!["a", "b'c", "d'e"]
        );
        assert_eq!(parse_mysql_enum_list("SET('x','y')"), vec!["x", "y"]);
        assert!(parse_mysql_enum_list("varchar(10)").is_empty());
    }

    #[test]
    fn check_in_list_bracketed() {
        assert_eq!(
            enum_values_from_check("Status", "([Status] IN ('a','b'))"),
            Some(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            enum_values_from_check("status", "`status` IN ('open', 'closed')"),
            Some(vec!["open".into(), "closed".into()])
        );
        assert_eq!(
            enum_values_from_check("status", "\"status\" IN ('a')"),
            Some(vec!["a".into()])
        );
        assert_eq!(
            enum_values_from_check("status", "status IN ('a','b')"),
            Some(vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn check_or_equals_form() {
        assert_eq!(
            enum_values_from_check("Status", "([Status]='a' OR [Status]='b')"),
            Some(vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn check_pg_any_array_form() {
        assert_eq!(
            enum_values_from_check(
                "status",
                "CHECK ((status = ANY (ARRAY['a'::text, 'b'::text])))"
            ),
            Some(vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn check_other_column_is_none() {
        assert_eq!(
            enum_values_from_check("Status", "([Other] IN ('a','b'))"),
            None
        );
    }

    #[test]
    fn check_multi_column_is_none() {
        assert_eq!(
            enum_values_from_check("Status", "([Status] IN ('a','b') AND [Age]>(0))"),
            None
        );
        assert_eq!(
            enum_values_from_check("Status", "([Status]='a' OR [Other]='b')"),
            None
        );
    }
}
