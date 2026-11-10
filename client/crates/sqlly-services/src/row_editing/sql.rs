//! Pure T-SQL construction for the row editor: identifier quoting, value
//! rendering, and the parameterized statement builders for the guarded
//! UPDATE/INSERT/DELETE, the single-row fetch, FK autocomplete search, and
//! the bounded related-rows reads. Everything here is a pure function, unit
//! tested without a server; the engine-neutral builders for
//! PostgreSQL/MySQL/SQLite live in `row_editing_sql`.

use sqlly_core::{CellValue, DatabaseType, MaterializedResultSet, SqlParam};

use super::types::{
    DeleteRowRequest, FkCandidate, FkLabelRequest, FkLabelResult, FkSearchRequest, FkSearchResult,
    InsertRowRequest, RelatedRowsRequest, TableTarget, UpdateRowRequest,
};
use crate::row_editing_sql::{build_fk_labels_sql_for, expression_sql};

/// Brackets a SQL Server identifier, doubling any `]`.
pub fn quote_ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// Appends `expr = @Pn` (binding `value`) or `expr IS NULL` to a predicate
/// list. NULLs are rendered into the SQL text so parameters never need typed
/// NULL binding.
pub(crate) fn push_predicate(
    predicates: &mut Vec<String>,
    params: &mut Vec<SqlParam>,
    column: &str,
    value: &CellValue,
) {
    if matches!(value, CellValue::Null) {
        predicates.push(format!("{} IS NULL", quote_ident(column)));
    } else {
        params.push(SqlParam::from(value));
        predicates.push(format!("{} = @P{}", quote_ident(column), params.len()));
    }
}

/// Builds the parameterized guarded UPDATE for a request. The WHERE clause
/// covers the key plus the rowversion when available, otherwise the original
/// values of every changed column (optimistic concurrency fallback).
pub fn build_update_sql(request: &UpdateRowRequest) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let mut assignments: Vec<String> = Vec::new();
    for (column, value) in &request.changes {
        if matches!(value, CellValue::Null) {
            assignments.push(format!("{} = NULL", quote_ident(column)));
        } else {
            params.push(SqlParam::from(value));
            assignments.push(format!("{} = @P{}", quote_ident(column), params.len()));
        }
    }
    for (column, expr) in &request.expressions {
        assignments.push(format!(
            "{} = {}",
            quote_ident(column),
            expression_sql(DatabaseType::SqlServer, *expr)
        ));
    }

    let mut predicates: Vec<String> = Vec::new();
    for (column, value) in &request.key {
        push_predicate(&mut predicates, &mut params, column, value);
    }
    match &request.rowversion {
        Some((column, value)) => push_predicate(&mut predicates, &mut params, column, value),
        None => {
            for (column, value) in &request.original_values {
                push_predicate(&mut predicates, &mut params, column, value);
            }
        }
    }

    let sql = format!(
        "UPDATE {} SET {} WHERE {}",
        request.target.quoted(),
        assignments.join(", "),
        predicates.join(" AND ")
    );
    (sql, params)
}

/// Builds the parameterized guarded INSERT. Uses `OUTPUT INSERTED.*` so the
/// caller gets identity/computed/default values back in one round trip. With no
/// supplied values, emits `DEFAULT VALUES`.
pub fn build_insert_sql(request: &InsertRowRequest) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let table = request.target.quoted();
    if request.values.is_empty() && request.expressions.is_empty() {
        return (
            format!("INSERT INTO {table} OUTPUT INSERTED.* DEFAULT VALUES"),
            params,
        );
    }
    let mut columns: Vec<String> =
        Vec::with_capacity(request.values.len() + request.expressions.len());
    let mut placeholders: Vec<String> =
        Vec::with_capacity(request.values.len() + request.expressions.len());
    for (column, value) in &request.values {
        columns.push(quote_ident(column));
        if matches!(value, CellValue::Null) {
            placeholders.push("NULL".to_string());
        } else {
            params.push(SqlParam::from(value));
            placeholders.push(format!("@P{}", params.len()));
        }
    }
    for (column, expr) in &request.expressions {
        columns.push(quote_ident(column));
        placeholders.push(expression_sql(DatabaseType::SqlServer, *expr).to_string());
    }
    let sql = format!(
        "INSERT INTO {table} ({}) OUTPUT INSERTED.* VALUES ({})",
        columns.join(", "),
        placeholders.join(", ")
    );
    (sql, params)
}

/// Builds the parameterized guarded DELETE. The WHERE clause covers the key
/// plus the rowversion when available, otherwise the original values of every
/// column (optimistic concurrency fallback).
pub fn build_delete_sql(request: &DeleteRowRequest) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let mut predicates: Vec<String> = Vec::new();
    for (column, value) in &request.key {
        push_predicate(&mut predicates, &mut params, column, value);
    }
    match &request.rowversion {
        Some((column, value)) => push_predicate(&mut predicates, &mut params, column, value),
        None => {
            for (column, value) in &request.original_values {
                push_predicate(&mut predicates, &mut params, column, value);
            }
        }
    }
    let sql = format!(
        "DELETE FROM {} WHERE {}",
        request.target.quoted(),
        predicates.join(" AND ")
    );
    (sql, params)
}

/// Builds the parameterized single-row fetch by key.
pub fn build_select_row_sql(
    target: &TableTarget,
    key: &[(String, CellValue)],
) -> (String, Vec<SqlParam>) {
    let mut params: Vec<SqlParam> = Vec::new();
    let mut predicates: Vec<String> = Vec::new();
    for (column, value) in key {
        push_predicate(&mut predicates, &mut params, column, value);
    }
    let sql = format!(
        "SELECT * FROM {} WHERE {}",
        target.quoted(),
        predicates.join(" AND ")
    );
    (sql, params)
}

/// Renders a cell value as a compact human string (FK labels, key fallbacks).
pub fn render_cell(value: &CellValue) -> String {
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => f.to_string(),
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(b) => format!(
            "0x{}",
            b.iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ),
    }
}

/// Escapes `LIKE` metacharacters (`%`, `_`, `[`) so a user's filter text is
/// matched literally. Pairs with `ESCAPE '\'` in the generated predicate.
pub fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '%' | '_' | '[' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Builds the bounded, parameterized referenced-table search for FK
/// autocomplete. Reads `limit + 1` rows so the caller can detect truncation.
/// Filters on the display column when present, otherwise on the (single) key
/// column cast to text; an empty query returns the first rows ordered by the
/// display/first-key column.
pub fn build_fk_search_sql(request: &FkSearchRequest) -> (String, Vec<SqlParam>) {
    let limit = request.limit.max(1);
    let mut selected: Vec<String> = request.key_columns.iter().map(|c| quote_ident(c)).collect();
    let order_by = match &request.display_column {
        Some(display) => {
            selected.push(quote_ident(display));
            quote_ident(display)
        }
        None => selected.first().cloned().unwrap_or_else(|| "1".to_string()),
    };

    // Filter target: the display column if present, else the first key column.
    let filter_column = request
        .display_column
        .as_ref()
        .map(|d| quote_ident(d))
        .or_else(|| request.key_columns.first().map(|c| quote_ident(c)));

    let mut params: Vec<SqlParam> = Vec::new();
    let where_clause = match (filter_column, request.query.trim().is_empty()) {
        (Some(column), false) => {
            params.push(SqlParam::String(format!(
                "%{}%",
                escape_like(request.query.trim())
            )));
            format!(" WHERE CONVERT(nvarchar(4000), {column}) LIKE @P1 ESCAPE '\\'")
        }
        _ => String::new(),
    };

    let sql = format!(
        "SELECT TOP ({}) {} FROM {}{} ORDER BY {}",
        limit + 1,
        selected.join(", "),
        request.referenced.quoted(),
        where_clause,
        order_by,
    );
    (sql, params)
}

/// Parses an FK-search result set into candidates, given how many leading
/// columns are the key and whether a trailing display column is present.
pub fn parse_fk_candidates(
    set: &MaterializedResultSet,
    key_count: usize,
    has_display: bool,
    limit: usize,
) -> FkSearchResult {
    let truncated = set.rows.len() > limit;
    let candidates = set
        .rows
        .iter()
        .take(limit)
        .map(|row| {
            let key: Vec<CellValue> = (0..key_count)
                .map(|i| row.get(i).cloned().unwrap_or(CellValue::Null))
                .collect();
            let label = if has_display {
                match row.get(key_count) {
                    Some(CellValue::Null) | None => {
                        key.iter().map(render_cell).collect::<Vec<_>>().join(", ")
                    }
                    Some(display) => render_cell(display),
                }
            } else {
                key.iter().map(render_cell).collect::<Vec<_>>().join(", ")
            };
            FkCandidate { key, label }
        })
        .collect();
    FkSearchResult {
        candidates,
        truncated,
    }
}

/// Builds a batched FK label lookup for TDS (`@P1`… placeholders).
pub fn build_fk_labels_sql(
    request: &FkLabelRequest,
) -> sqlly_core::StudioResult<(String, Vec<SqlParam>)> {
    build_fk_labels_sql_for(DatabaseType::SqlServer, request)
}

/// Parses a batched FK label lookup result set into `(key, label)` pairs.
pub fn parse_fk_labels(set: &MaterializedResultSet) -> FkLabelResult {
    let labels = set
        .rows
        .iter()
        .filter_map(|row| {
            let key = row.first()?.clone();
            let label = match row.get(1) {
                Some(CellValue::Null) | None => return None,
                Some(display) => render_cell(display),
            };
            Some((key, label))
        })
        .collect();
    FkLabelResult { labels }
}

/// Renders a cell as a SQL literal for inlining into an "Open query" statement
/// (which is not parameterized). Strings are quoted with `N'...'` and doubled
/// quotes; binary uses `0x…`; NULL is `NULL`.
pub fn sql_literal(value: &CellValue) -> String {
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => f.to_string(),
        CellValue::String(s) => format!("N'{}'", s.replace('\'', "''")),
        CellValue::Bytes(b) => format!(
            "0x{}",
            b.iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ),
    }
}

/// Builds the bounded parameterized fetch for related rows (`TOP (limit + 1)`
/// so the caller can flag truncation).
pub fn build_related_rows_sql(request: &RelatedRowsRequest) -> (String, Vec<SqlParam>) {
    let limit = request.limit.max(1);
    let mut params: Vec<SqlParam> = Vec::new();
    let mut predicates: Vec<String> = Vec::new();
    for (column, value) in &request.predicates {
        push_predicate(&mut predicates, &mut params, column, value);
    }
    let where_clause = if predicates.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", predicates.join(" AND "))
    };
    let sql = format!(
        "SELECT TOP ({}) * FROM {}{}",
        limit + 1,
        request.table.quoted(),
        where_clause
    );
    (sql, params)
}

/// Builds the full (untruncated, value-inlined) query for the "Open query"
/// action, safe to drop into the editor.
pub fn build_related_open_sql(request: &RelatedRowsRequest) -> String {
    let predicates: Vec<String> = request
        .predicates
        .iter()
        .map(|(column, value)| {
            if matches!(value, CellValue::Null) {
                format!("{} IS NULL", quote_ident(column))
            } else {
                format!("{} = {}", quote_ident(column), sql_literal(value))
            }
        })
        .collect();
    let where_clause = if predicates.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", predicates.join(" AND "))
    };
    format!("SELECT * FROM {}{};", request.table.quoted(), where_clause)
}

#[cfg(test)]
mod tests {
    use super::super::types::ColumnExpression;
    use super::*;
    use sqlly_core::ResultColumn;

    fn column(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: "int".to_string(),
            nullable: true,
        }
    }

    #[test]
    fn quote_ident_doubles_closing_brackets() {
        assert_eq!(quote_ident("Orders"), "[Orders]");
        assert_eq!(quote_ident("a]b"), "[a]]b]");
    }

    #[test]
    fn update_sql_uses_rowversion_when_present() {
        let request = UpdateRowRequest {
            target: TableTarget {
                schema: "dbo".into(),
                table: "Orders".into(),
            },
            key: vec![("Id".to_string(), CellValue::I64(7))],
            rowversion: Some(("RV".to_string(), CellValue::Bytes(vec![1, 2]))),
            changes: vec![("Name".to_string(), CellValue::String("x".into()))],
            original_values: vec![("Name".to_string(), CellValue::String("old".into()))],
            expressions: Vec::new(),
        };
        let (sql, params) = build_update_sql(&request);
        assert_eq!(
            sql,
            "UPDATE [dbo].[Orders] SET [Name] = @P1 WHERE [Id] = @P2 AND [RV] = @P3"
        );
        assert_eq!(
            params,
            vec![
                SqlParam::String("x".into()),
                SqlParam::I64(7),
                SqlParam::Bytes(vec![1, 2]),
            ]
        );
    }

    #[test]
    fn update_sql_falls_back_to_original_values_without_rowversion() {
        let request = UpdateRowRequest {
            target: TableTarget {
                schema: "dbo".into(),
                table: "Orders".into(),
            },
            key: vec![("Id".to_string(), CellValue::I64(7))],
            rowversion: None,
            changes: vec![("Name".to_string(), CellValue::String("x".into()))],
            original_values: vec![("Name".to_string(), CellValue::Null)],
            expressions: Vec::new(),
        };
        let (sql, params) = build_update_sql(&request);
        assert_eq!(
            sql,
            "UPDATE [dbo].[Orders] SET [Name] = @P1 WHERE [Id] = @P2 AND [Name] IS NULL"
        );
        assert_eq!(params, vec![SqlParam::String("x".into()), SqlParam::I64(7)]);
    }

    #[test]
    fn update_sql_renders_null_assignment_literally() {
        let request = UpdateRowRequest {
            target: TableTarget {
                schema: "dbo".into(),
                table: "T".into(),
            },
            key: vec![("Id".to_string(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("A".to_string(), CellValue::Null)],
            original_values: vec![("A".to_string(), CellValue::String("was".into()))],
            expressions: Vec::new(),
        };
        let (sql, params) = build_update_sql(&request);
        assert_eq!(
            sql,
            "UPDATE [dbo].[T] SET [A] = NULL WHERE [Id] = @P1 AND [A] = @P2"
        );
        assert_eq!(
            params,
            vec![SqlParam::I64(1), SqlParam::String("was".into())]
        );
    }

    #[test]
    fn insert_sql_outputs_inserted_and_renders_null_literally() {
        let request = InsertRowRequest {
            target: TableTarget {
                schema: "dbo".into(),
                table: "Person".into(),
            },
            values: vec![
                ("Name".to_string(), CellValue::String("Ada".into())),
                ("Note".to_string(), CellValue::Null),
                ("Age".to_string(), CellValue::I64(37)),
            ],
            expressions: Vec::new(),
        };
        let (sql, params) = build_insert_sql(&request);
        assert_eq!(
            sql,
            "INSERT INTO [dbo].[Person] ([Name], [Note], [Age]) \
             OUTPUT INSERTED.* VALUES (@P1, NULL, @P2)"
        );
        assert_eq!(
            params,
            vec![SqlParam::String("Ada".into()), SqlParam::I64(37)]
        );
    }

    #[test]
    fn insert_sql_uses_default_values_when_empty() {
        let request = InsertRowRequest {
            target: TableTarget {
                schema: "dbo".into(),
                table: "T".into(),
            },
            values: vec![],
            expressions: Vec::new(),
        };
        let (sql, params) = build_insert_sql(&request);
        assert_eq!(
            sql,
            "INSERT INTO [dbo].[T] OUTPUT INSERTED.* DEFAULT VALUES"
        );
        assert!(params.is_empty());
    }

    #[test]
    fn update_renders_default_and_server_time() {
        let request = UpdateRowRequest {
            target: TableTarget {
                schema: "dbo".into(),
                table: "T".into(),
            },
            key: vec![("Id".to_string(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("a".to_string(), CellValue::String("x".into()))],
            original_values: vec![("a".to_string(), CellValue::String("old".into()))],
            expressions: vec![
                ("b".to_string(), ColumnExpression::Default),
                ("c".to_string(), ColumnExpression::ServerTime),
            ],
        };
        let (sql, params) = build_update_sql(&request);
        assert_eq!(
            sql,
            "UPDATE [dbo].[T] SET [a] = @P1, [b] = DEFAULT, [c] = SYSDATETIME() WHERE [Id] = @P2 AND [a] = @P3"
        );
        assert_eq!(
            params,
            vec![
                SqlParam::String("x".into()),
                SqlParam::I64(1),
                SqlParam::String("old".into()),
            ]
        );
    }

    #[test]
    fn insert_with_only_expressions_lists_them() {
        let request = InsertRowRequest {
            target: TableTarget {
                schema: "dbo".into(),
                table: "T".into(),
            },
            values: vec![],
            expressions: vec![
                ("b".to_string(), ColumnExpression::Default),
                ("c".to_string(), ColumnExpression::ServerTime),
            ],
        };
        let (sql, params) = build_insert_sql(&request);
        assert_eq!(
            sql,
            "INSERT INTO [dbo].[T] ([b], [c]) OUTPUT INSERTED.* VALUES (DEFAULT, SYSDATETIME())"
        );
        assert!(params.is_empty());
        assert!(!sql.contains("DEFAULT VALUES"));
    }

    #[test]
    fn delete_sql_uses_rowversion_when_present() {
        let request = DeleteRowRequest {
            target: TableTarget {
                schema: "dbo".into(),
                table: "T".into(),
            },
            key: vec![("Id".to_string(), CellValue::I64(3))],
            rowversion: Some(("RV".to_string(), CellValue::Bytes(vec![9]))),
            original_values: vec![("Name".to_string(), CellValue::String("x".into()))],
        };
        let (sql, params) = build_delete_sql(&request);
        assert_eq!(sql, "DELETE FROM [dbo].[T] WHERE [Id] = @P1 AND [RV] = @P2");
        assert_eq!(params, vec![SqlParam::I64(3), SqlParam::Bytes(vec![9])]);
    }

    #[test]
    fn delete_sql_falls_back_to_original_values() {
        let request = DeleteRowRequest {
            target: TableTarget {
                schema: "dbo".into(),
                table: "T".into(),
            },
            key: vec![("Id".to_string(), CellValue::I64(3))],
            rowversion: None,
            original_values: vec![
                ("Name".to_string(), CellValue::String("x".into())),
                ("Note".to_string(), CellValue::Null),
            ],
        };
        let (sql, params) = build_delete_sql(&request);
        assert_eq!(
            sql,
            "DELETE FROM [dbo].[T] WHERE [Id] = @P1 AND [Name] = @P2 AND [Note] IS NULL"
        );
        assert_eq!(params, vec![SqlParam::I64(3), SqlParam::String("x".into())]);
    }

    #[test]
    fn select_row_sql_binds_key_in_order() {
        let target = TableTarget {
            schema: "s".into(),
            table: "t".into(),
        };
        let key = vec![
            ("A".to_string(), CellValue::I64(1)),
            ("B".to_string(), CellValue::String("x".into())),
        ];
        let (sql, params) = build_select_row_sql(&target, &key);
        assert_eq!(sql, "SELECT * FROM [s].[t] WHERE [A] = @P1 AND [B] = @P2");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn fk_search_sql_filters_on_display_column_with_escaped_like() {
        let request = FkSearchRequest {
            referenced: TableTarget {
                schema: "dbo".into(),
                table: "Customer".into(),
            },
            key_columns: vec!["Id".into()],
            display_column: Some("Name".into()),
            query: "a%c".into(),
            limit: 50,
        };
        let (sql, params) = build_fk_search_sql(&request);
        assert_eq!(
            sql,
            "SELECT TOP (51) [Id], [Name] FROM [dbo].[Customer] \
             WHERE CONVERT(nvarchar(4000), [Name]) LIKE @P1 ESCAPE '\\' ORDER BY [Name]"
        );
        assert_eq!(params, vec![SqlParam::String("%a\\%c%".into())]);
    }

    #[test]
    fn fk_search_sql_without_query_or_display_orders_by_key() {
        let request = FkSearchRequest {
            referenced: TableTarget {
                schema: "dbo".into(),
                table: "Customer".into(),
            },
            key_columns: vec!["Id".into()],
            display_column: None,
            query: "   ".into(),
            limit: 10,
        };
        let (sql, params) = build_fk_search_sql(&request);
        assert_eq!(
            sql,
            "SELECT TOP (11) [Id] FROM [dbo].[Customer] ORDER BY [Id]"
        );
        assert!(params.is_empty());
    }

    #[test]
    fn parse_fk_candidates_prefers_display_and_flags_truncation() {
        let set = MaterializedResultSet {
            columns: vec![column("Id"), column("Name")],
            rows: vec![
                vec![CellValue::I64(1), CellValue::String("Acme".into())],
                vec![CellValue::I64(2), CellValue::Null],
                vec![CellValue::I64(3), CellValue::String("Beta".into())],
            ],
        };
        let result = parse_fk_candidates(&set, 1, true, 2);
        assert!(result.truncated, "3 rows over a limit of 2");
        assert_eq!(result.candidates.len(), 2);
        assert_eq!(result.candidates[0].label, "Acme");
        assert_eq!(result.candidates[0].key, vec![CellValue::I64(1)]);
        // Null display falls back to the rendered key.
        assert_eq!(result.candidates[1].label, "2");
    }

    #[test]
    fn escape_like_escapes_metacharacters() {
        assert_eq!(escape_like("a%b_c[d"), "a\\%b\\_c\\[d");
        assert_eq!(escape_like("plain"), "plain");
    }

    #[test]
    fn related_rows_sql_is_bounded_and_parameterized() {
        let request = RelatedRowsRequest {
            table: TableTarget {
                schema: "dbo".into(),
                table: "Orders".into(),
            },
            predicates: vec![("CustomerId".into(), CellValue::I64(7))],
            limit: 100,
        };
        let (sql, params) = build_related_rows_sql(&request);
        assert_eq!(
            sql,
            "SELECT TOP (101) * FROM [dbo].[Orders] WHERE [CustomerId] = @P1"
        );
        assert_eq!(params, vec![SqlParam::I64(7)]);
    }

    #[test]
    fn related_open_sql_inlines_literals_safely() {
        let request = RelatedRowsRequest {
            table: TableTarget {
                schema: "dbo".into(),
                table: "Orders".into(),
            },
            predicates: vec![
                ("Code".into(), CellValue::String("O'Neil".into())),
                ("Region".into(), CellValue::Null),
            ],
            limit: 50,
        };
        let sql = build_related_open_sql(&request);
        assert_eq!(
            sql,
            "SELECT * FROM [dbo].[Orders] WHERE [Code] = N'O''Neil' AND [Region] IS NULL;"
        );
    }

    #[test]
    fn sql_literal_quotes_and_escapes() {
        assert_eq!(sql_literal(&CellValue::I64(3)), "3");
        assert_eq!(sql_literal(&CellValue::Null), "NULL");
        assert_eq!(sql_literal(&CellValue::Bool(true)), "1");
        assert_eq!(sql_literal(&CellValue::String("a'b".into())), "N'a''b'");
        assert_eq!(sql_literal(&CellValue::Bytes(vec![0xAB, 0x01])), "0xAB01");
    }

    #[test]
    fn fk_labels_sql_tds_uses_at_params() {
        let request = FkLabelRequest {
            referenced: TableTarget {
                schema: "dbo".into(),
                table: "Customer".into(),
            },
            key_column: "Id".into(),
            display_column: "Name".into(),
            keys: vec![CellValue::I64(7), CellValue::I64(8)],
        };
        let (sql, params) = build_fk_labels_sql(&request).expect("sql");
        assert_eq!(
            sql,
            "SELECT [Id], CAST([Name] AS NVARCHAR(MAX)) FROM [dbo].[Customer] \
             WHERE [Id] IN (@P1, @P2)"
        );
        assert_eq!(params, vec![SqlParam::I64(7), SqlParam::I64(8)]);
    }
}
