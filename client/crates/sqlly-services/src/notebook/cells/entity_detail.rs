//! Entity detail cell spec and the read-only, row-limited queries that
//! fetch the rows related to one selected row.
//!
//! ```json
//! {"source": {"cell": "customers", "row": 0},
//!  "table": {"schema": "dbo", "name": "Customers"},
//!  "relationships": [
//!    {"name": "FK_Orders_Customers", "direction": "incoming",
//!     "schema": "dbo", "table": "Orders", "columns": [["CustomerID", "CustomerID"]]}],
//!  "limit": 50}
//! ```
//!
//! `source` is a cell's row (`row` is the selected row, 0-based) or
//! `{"parameter": "<name>", "column": "<key column>"}`. Only the
//! relationships listed (the ones the user approved) are queried.
//! `columns` pairs are `[column in the selected row, column in the related
//! table]`. `direction` is `outgoing` (this table's foreign key, the parent
//! row) or `incoming` (rows of another table that point here).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_intel::intellisense::FkRelation;
use sqlly_intel::query::safety::classify_select_only_for;
use sqlly_lang::sqlmodel::DatabaseType;

use super::unknown_fields;
use crate::identifier_quoting::qualified_identifier;
use crate::notebook::params::{ParamType, ParamValue, ParameterDef, render_literal};

const KNOWN: &[&str] = &["source", "table", "relationships", "limit", "input"];

/// Rows fetched per relationship by default, and at most.
pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 500;
/// Most relationships one cell queries.
pub const MAX_RELATIONSHIPS: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntitySource {
    Row { cell: String, row: usize },
    Parameter { name: String, column: String },
}

impl Default for EntitySource {
    fn default() -> Self {
        EntitySource::Row {
            cell: String::new(),
            row: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TableName {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub name: String,
}

impl TableName {
    pub fn quoted(&self, db: DatabaseType) -> String {
        qualified_identifier(db, self.schema.as_deref(), &self.name)
    }

    pub fn display(&self) -> String {
        match &self.schema {
            Some(s) if !s.is_empty() => format!("{s}.{}", self.name),
            _ => self.name.clone(),
        }
    }

    pub fn matches(&self, schema: &str, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
            && self
                .schema
                .as_deref()
                .is_none_or(|s| s.is_empty() || s.eq_ignore_ascii_case(schema))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Outgoing,
    Incoming,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relationship {
    #[serde(default)]
    pub name: String,
    pub direction: Direction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub table: String,
    /// `[selected-row column, related-table column]` pairs.
    pub columns: Vec<(String, String)>,
}

impl Relationship {
    pub fn related_table(&self) -> TableName {
        TableName {
            schema: self.schema.clone(),
            name: self.table.clone(),
        }
    }

    /// Stable id for the UI and for approving a relationship.
    pub fn key(&self) -> String {
        let cols: Vec<String> = self
            .columns
            .iter()
            .map(|(a, b)| format!("{a}={b}"))
            .collect();
        format!(
            "{}:{}:{}",
            match self.direction {
                Direction::Outgoing => "out",
                Direction::Incoming => "in",
            },
            self.related_table().display(),
            cols.join(",")
        )
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EntityDetailSpec {
    pub source: EntitySource,
    pub table: TableName,
    pub relationships: Vec<Relationship>,
    pub limit: Option<usize>,
    pub extra: Map<String, Value>,
}

impl EntityDetailSpec {
    pub fn from_spec(spec: &Value) -> Self {
        let source = spec
            .get("source")
            .and_then(|s| {
                if let Some(name) = s.get("parameter").and_then(Value::as_str) {
                    return Some(EntitySource::Parameter {
                        name: name.to_string(),
                        column: s
                            .get("column")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
                Some(EntitySource::Row {
                    cell: s.get("cell")?.as_str()?.to_string(),
                    row: s.get("row").and_then(Value::as_u64).unwrap_or(0) as usize,
                })
            })
            .or_else(|| {
                spec.get("input")
                    .and_then(Value::as_str)
                    .map(|cell| EntitySource::Row {
                        cell: cell.to_string(),
                        row: 0,
                    })
            })
            .unwrap_or_default();
        Self {
            source,
            table: spec
                .get("table")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default(),
            relationships: spec
                .get("relationships")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|r| serde_json::from_value(r.clone()).ok())
                        .collect()
                })
                .unwrap_or_default(),
            limit: spec
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
            extra: unknown_fields(spec, KNOWN),
        }
    }

    /// The source cell is also written as `input`, so the dependency graph
    /// sees the edge.
    pub fn to_spec(&self) -> Value {
        let mut map = self.extra.clone();
        let source = match &self.source {
            EntitySource::Row { cell, row } => {
                map.insert("input".into(), Value::String(cell.clone()));
                serde_json::json!({ "cell": cell, "row": row })
            }
            EntitySource::Parameter { name, column } => {
                serde_json::json!({ "parameter": name, "column": column })
            }
        };
        map.insert("source".into(), source);
        map.insert(
            "table".into(),
            serde_json::to_value(&self.table).unwrap_or(Value::Null),
        );
        map.insert(
            "relationships".into(),
            serde_json::to_value(&self.relationships).unwrap_or(Value::Null),
        );
        if let Some(l) = self.limit {
            map.insert("limit".into(), l.into());
        }
        Value::Object(map)
    }

    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }

    pub fn source_cell(&self) -> Option<&str> {
        match &self.source {
            EntitySource::Row { cell, .. } if !cell.is_empty() => Some(cell),
            _ => None,
        }
    }
}

/// Every relationship the catalog knows for `table`: its own foreign keys
/// (outgoing) and other tables' keys that point at it (incoming).
/// `objects` yields `(schema, table, foreign keys)`.
pub fn catalog_relationships<'a>(
    table: &TableName,
    objects: impl IntoIterator<Item = (&'a str, &'a str, &'a [FkRelation])>,
) -> Vec<Relationship> {
    let mut out = Vec::new();
    for (schema, name, fks) in objects {
        let is_self = table.matches(schema, name);
        for key in FkRelation::group(fks) {
            let first = key[0];
            if is_self {
                out.push(Relationship {
                    name: first.constraint.clone().unwrap_or_default(),
                    direction: Direction::Outgoing,
                    schema: Some(first.target_schema.clone()).filter(|s| !s.is_empty()),
                    table: first.target_table.clone(),
                    columns: key
                        .iter()
                        .map(|r| (r.column.clone(), r.target_column.clone()))
                        .collect(),
                });
            }
            if table.matches(&first.target_schema, &first.target_table) {
                out.push(Relationship {
                    name: first.constraint.clone().unwrap_or_default(),
                    direction: Direction::Incoming,
                    schema: Some(schema.to_string()).filter(|s| !s.is_empty()),
                    table: name.to_string(),
                    columns: key
                        .iter()
                        .map(|r| (r.target_column.clone(), r.column.clone()))
                        .collect(),
                });
            }
        }
    }
    out
}

/// Why a related-rows query was not built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailQueryError {
    /// The selected row has no column of this name.
    MissingColumn(String),
    /// A key value is NULL, so nothing can be related.
    NullKey(String),
    /// A value of this type cannot be written as a key literal.
    UnsupportedValue(String),
    /// The built SQL did not classify as read-only (never expected).
    NotReadOnly,
}

fn literal(db: DatabaseType, value: &CellValue, column: &str) -> Result<String, DetailQueryError> {
    let (ty, param) = match value {
        CellValue::Null => return Err(DetailQueryError::NullKey(column.to_string())),
        CellValue::I64(n) => (ParamType::Integer, ParamValue::Integer(*n)),
        CellValue::F64(f) if f.is_finite() => {
            (ParamType::Decimal, ParamValue::Decimal(f.to_string()))
        }
        CellValue::String(s) => (ParamType::String, ParamValue::Text(s.clone())),
        CellValue::Bool(b) => (ParamType::Boolean, ParamValue::Boolean(*b)),
        _ => return Err(DetailQueryError::UnsupportedValue(column.to_string())),
    };
    render_literal(db, &ParameterDef::new("key", ty), &param, None)
        .map_err(|_| DetailQueryError::UnsupportedValue(column.to_string()))
}

/// The SELECT for one relationship: every column of the related table
/// where its key columns equal the selected row's, at most `limit + 1`
/// rows (the extra row tells the caller the list was cut).
pub fn related_rows_sql(
    db: DatabaseType,
    relationship: &Relationship,
    columns: &[ResultColumn],
    row: &[CellValue],
    limit: usize,
) -> Result<String, DetailQueryError> {
    let limit = limit.clamp(1, MAX_LIMIT);
    let mut predicates = Vec::new();
    for (local, remote) in &relationship.columns {
        let index = columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(local))
            .ok_or_else(|| DetailQueryError::MissingColumn(local.clone()))?;
        let value = row
            .get(index)
            .ok_or_else(|| DetailQueryError::MissingColumn(local.clone()))?;
        predicates.push(format!(
            "{} = {}",
            crate::identifier_quoting::quote_identifier(db, remote),
            literal(db, value, local)?
        ));
    }
    let (prefix, suffix) = db.row_limit_clauses(limit + 1);
    let mut sql = format!(
        "SELECT {prefix}* FROM {} WHERE {}",
        relationship.related_table().quoted(db),
        predicates.join(" AND ")
    );
    if !suffix.is_empty() {
        sql.push(' ');
        sql.push_str(&suffix);
    }
    if !classify_select_only_for(db, &sql).select_only {
        return Err(DetailQueryError::NotReadOnly);
    }
    Ok(sql)
}

/// The selected row's key from a parameter value: one column.
pub fn parameter_row(column: &str, value: &ParamValue) -> (Vec<ResultColumn>, Vec<CellValue>) {
    let cell = match value {
        ParamValue::Integer(n) => CellValue::I64(*n),
        ParamValue::Boolean(b) => CellValue::Bool(*b),
        ParamValue::Null => CellValue::Null,
        ParamValue::Text(s) | ParamValue::Decimal(s) | ParamValue::Identifier(s) => {
            CellValue::String(s.clone())
        }
        other => CellValue::String(other.to_json().to_string()),
    };
    (
        vec![ResultColumn {
            name: column.to_string(),
            data_type: String::new(),
            nullable: true,
        }],
        vec![cell],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: "int".into(),
            nullable: true,
        }
    }

    fn orders() -> Relationship {
        Relationship {
            name: "FK_Orders_Customers".into(),
            direction: Direction::Incoming,
            schema: Some("dbo".into()),
            table: "Orders".into(),
            columns: vec![("CustomerID".into(), "CustomerID".into())],
        }
    }

    #[test]
    fn spec_round_trips_and_names_its_input() {
        let spec = EntityDetailSpec {
            source: EntitySource::Row {
                cell: "customers".into(),
                row: 3,
            },
            table: TableName {
                schema: Some("dbo".into()),
                name: "Customers".into(),
            },
            relationships: vec![orders()],
            limit: Some(10_000),
            extra: Map::new(),
        };
        let json = spec.to_spec();
        assert_eq!(json["input"], "customers");
        let back = EntityDetailSpec::from_spec(&json);
        assert_eq!(back.source, spec.source);
        assert_eq!(back.relationships, spec.relationships);
        assert_eq!(back.limit(), MAX_LIMIT);
        let p = EntityDetailSpec::from_spec(
            &json!({"source": {"parameter": "cid", "column": "CustomerID"}}),
        );
        assert!(matches!(p.source, EntitySource::Parameter { .. }));
        assert_eq!(p.limit(), DEFAULT_LIMIT);
        assert!(p.source_cell().is_none());
    }

    #[test]
    fn related_queries_are_read_only_limited_and_quoted() {
        let cols = [col("CustomerID"), col("Name")];
        let row = [
            CellValue::String("AL'FKI".into()),
            CellValue::String("x".into()),
        ];
        for db in [
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::Sqlite,
            DatabaseType::Oracle,
        ] {
            let sql = related_rows_sql(db, &orders(), &cols, &row, 50).unwrap();
            assert!(sql.starts_with("SELECT "), "{sql}");
            assert!(classify_select_only_for(db, &sql).select_only, "{sql}");
            assert!(sql.contains("51"), "{sql}");
            assert!(!sql.contains("AL'FKI"), "{sql}");
        }
        let tsql =
            related_rows_sql(DatabaseType::SqlServer, &orders(), &cols, &row, 9_999).unwrap();
        assert!(
            tsql.starts_with(
                "SELECT TOP 501 * FROM [dbo].[Orders] WHERE [CustomerID] = N'AL''FKI'"
            ),
            "{tsql}"
        );
        let hostile = Relationship {
            table: "Orders]; DROP TABLE x; --".into(),
            ..orders()
        };
        let sql = related_rows_sql(DatabaseType::SqlServer, &hostile, &cols, &row, 5).unwrap();
        assert!(classify_select_only_for(DatabaseType::SqlServer, &sql).select_only);
        assert_eq!(
            related_rows_sql(
                DatabaseType::SqlServer,
                &orders(),
                &cols,
                &[CellValue::Null, CellValue::Null],
                5
            ),
            Err(DetailQueryError::NullKey("CustomerID".into()))
        );
        assert_eq!(
            related_rows_sql(DatabaseType::SqlServer, &orders(), &[col("Other")], &row, 5),
            Err(DetailQueryError::MissingColumn("CustomerID".into()))
        );
    }

    #[test]
    fn catalog_relationships_find_both_directions() {
        let fk = |column: &str, schema: &str, table: &str, target: &str| FkRelation {
            column: column.into(),
            target_schema: schema.into(),
            target_table: table.into(),
            target_column: target.into(),
            constraint: Some(format!("FK_{column}")),
        };
        let orders_fks = vec![fk("CustomerID", "dbo", "Customers", "CustomerID")];
        let customer_fks = vec![fk("RegionID", "dbo", "Regions", "RegionID")];
        let table = TableName {
            schema: Some("dbo".into()),
            name: "Customers".into(),
        };
        let rels = catalog_relationships(
            &table,
            [
                ("dbo", "Orders", orders_fks.as_slice()),
                ("dbo", "Customers", customer_fks.as_slice()),
            ],
        );
        assert_eq!(rels.len(), 2);
        let incoming = rels
            .iter()
            .find(|r| r.direction == Direction::Incoming)
            .unwrap();
        assert_eq!(incoming.table, "Orders");
        assert_eq!(
            incoming.columns,
            vec![("CustomerID".into(), "CustomerID".into())]
        );
        let outgoing = rels
            .iter()
            .find(|r| r.direction == Direction::Outgoing)
            .unwrap();
        assert_eq!(outgoing.table, "Regions");
        assert_ne!(incoming.key(), outgoing.key());
        let (cols, row) = parameter_row("CustomerID", &ParamValue::Integer(7));
        assert_eq!(
            (cols[0].name.as_str(), &row[0]),
            ("CustomerID", &CellValue::I64(7))
        );
    }
}
