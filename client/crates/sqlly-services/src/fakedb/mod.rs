//! An in-memory emulated database for demo builds.
//!
//! The wasm/browser build of SQLLY cannot open TDS connections to a real SQL
//! server (no raw sockets in the browser), so it needs a stand-in backend that
//! demonstrates how the system works: a catalog of tables, views, and stored
//! procedures, and a query executor able to answer a useful subset of SQL.
//!
//! This crate is deliberately dependency-light and free of async/network I/O
//! so it compiles for every target the workspace ships, including
//! `wasm32-unknown-unknown`. It is NOT a real SQL engine — it supports the
//! query shapes a demo needs (`SELECT` with projection, `WHERE`, `ORDER BY`,
//! `TOP`/`LIMIT`, views, `EXEC <procedure>`, and — via
//! [`FakeDatabase::execute_mut`] — simple `INSERT`/`UPDATE`/`DELETE`/
//! `CREATE TABLE`/`DROP TABLE`), returning typed result sets shaped like the
//! ones the real query path produces.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

mod demo;
mod parser;

pub use demo::demo_database;

/// A single cell value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl Value {
    /// Render the value the way a query tool displays it.
    pub fn display(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Text(t) => t.clone(),
        }
    }

    /// Ordering key used by `ORDER BY`. Numbers order numerically, text
    /// lexicographically; NULLs sort first (matching SQL Server).
    fn sort_key(&self) -> SortKey<'_> {
        match self {
            Value::Null => SortKey::Null,
            Value::Bool(b) => SortKey::Number(if *b { 1.0 } else { 0.0 }),
            Value::Int(i) => SortKey::Number(*i as f64),
            Value::Float(f) => SortKey::Number(*f),
            Value::Text(t) => SortKey::Text(t),
        }
    }
}

#[derive(PartialEq)]
enum SortKey<'a> {
    Null,
    Number(f64),
    Text(&'a str),
}

impl PartialOrd for SortKey<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        use SortKey::*;
        use std::cmp::Ordering;
        Some(match (self, other) {
            (Null, Null) => Ordering::Equal,
            (Null, _) => Ordering::Less,
            (_, Null) => Ordering::Greater,
            (Number(a), Number(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (Text(a), Text(b)) => a.cmp(b),
            // Mixed types: numbers before text, mirroring SQLite affinity.
            (Number(_), Text(_)) => Ordering::Less,
            (Text(_), Number(_)) => Ordering::Greater,
        })
    }
}

/// A column definition: name plus a display type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub type_name: String,
}

impl Column {
    pub fn new(name: &str, type_name: &str) -> Self {
        Self {
            name: name.to_string(),
            type_name: type_name.to_string(),
        }
    }
}

/// A table with data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

/// A view: a named `SELECT` over the tables, resolved at query time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct View {
    pub name: String,
    /// The `SELECT ...` text the view expands to (also shown as its DDL).
    pub definition: String,
    pub description: String,
}

/// A stored procedure descriptor. Execution is dispatched by name inside
/// [`FakeDatabase::execute`]; `body` is display-only DDL for the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Procedure {
    pub name: String,
    /// Parameter names (all optional text/number literals in this demo).
    pub params: Vec<String>,
    pub description: String,
    pub body: String,
}

/// One result set: columns + rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

impl ResultSet {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}

/// The outcome of executing a batch: zero or more result sets plus
/// informational messages (like the messages pane of a query tool).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub result_sets: Vec<ResultSet>,
    pub messages: Vec<String>,
}

/// Execution error with a human-readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeDbError(pub String);

impl fmt::Display for FakeDbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for FakeDbError {}

/// Catalog snapshot of everything the fake database exposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    pub tables: Vec<SchemaObject>,
    pub views: Vec<SchemaObject>,
    pub procedures: Vec<SchemaProcedure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaObject {
    pub name: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaProcedure {
    pub name: String,
    pub params: Vec<String>,
    pub description: String,
}

/// The emulated database: tables with data, views, and stored procedures.
#[derive(Debug, Clone, Default)]
pub struct FakeDatabase {
    tables: BTreeMap<String, Table>,
    views: BTreeMap<String, View>,
    procedures: BTreeMap<String, Procedure>,
}

impl FakeDatabase {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_table(&mut self, table: Table) {
        self.tables.insert(table.name.to_lowercase(), table);
    }

    pub fn add_view(&mut self, view: View) {
        self.views.insert(view.name.to_lowercase(), view);
    }

    pub fn add_procedure(&mut self, procedure: Procedure) {
        self.procedures
            .insert(procedure.name.to_lowercase(), procedure);
    }

    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(&name.to_lowercase())
    }

    pub fn view(&self, name: &str) -> Option<&View> {
        self.views.get(&name.to_lowercase())
    }

    pub fn procedure(&self, name: &str) -> Option<&Procedure> {
        self.procedures.get(&name.to_lowercase())
    }

    /// The full catalog, for schema-tree UIs. View columns are resolved by
    /// executing the view's definition against the current data.
    pub fn schema(&self) -> Schema {
        let tables = self
            .tables
            .values()
            .map(|t| SchemaObject {
                name: t.name.clone(),
                columns: t.columns.clone(),
            })
            .collect();
        let views = self
            .views
            .values()
            .map(|v| {
                let columns = self
                    .run_select(&v.definition)
                    .map(|rs| rs.columns)
                    .unwrap_or_default();
                SchemaObject {
                    name: v.name.clone(),
                    columns,
                }
            })
            .collect();
        let procedures = self
            .procedures
            .values()
            .map(|p| SchemaProcedure {
                name: p.name.clone(),
                params: p.params.clone(),
                description: p.description.clone(),
            })
            .collect();
        Schema {
            tables,
            views,
            procedures,
        }
    }

    /// Execute a SQL batch with mutation support: everything
    /// [`Self::execute`] answers, plus simple `INSERT`, `UPDATE`, `DELETE`,
    /// `CREATE TABLE`, and `DROP TABLE` against the in-memory store. This is
    /// the demo backend's normal path; the read-only [`Self::execute`] serves
    /// contexts that must not mutate.
    pub fn execute_mut(&mut self, sql: &str) -> Result<Execution, FakeDbError> {
        let mut execution = Execution {
            result_sets: Vec::new(),
            messages: Vec::new(),
        };
        let statements = parser::split_statements(sql);
        if statements.is_empty() {
            execution
                .messages
                .push("Commands completed successfully.".to_string());
            return Ok(execution);
        }
        for statement in statements {
            let trimmed = statement.trim();
            let first_word = trimmed
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_uppercase();
            match first_word.as_str() {
                "INSERT" => self.run_insert(trimmed, &mut execution)?,
                "UPDATE" => self.run_update(trimmed, &mut execution)?,
                "DELETE" => self.run_delete(trimmed, &mut execution)?,
                "CREATE" => self.run_create_table(trimmed, &mut execution)?,
                "DROP" => self.run_drop_table(trimmed, &mut execution)?,
                _ => self.execute_statement(trimmed, &mut execution)?,
            }
        }
        Ok(execution)
    }

    fn run_insert(&mut self, sql: &str, execution: &mut Execution) -> Result<(), FakeDbError> {
        let statement = parser::parse_insert(sql)?;
        let table = self
            .tables
            .get_mut(&statement.table.to_lowercase())
            .ok_or_else(|| FakeDbError(format!("Unknown table '{}'", statement.table)))?;
        // Column-name → position map; positional inserts must match arity.
        let positions: Vec<usize> = if statement.columns.is_empty() {
            (0..table.columns.len()).collect()
        } else {
            statement
                .columns
                .iter()
                .map(|name| {
                    table
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(name))
                        .ok_or_else(|| {
                            FakeDbError(format!("Unknown column '{name}' on '{}'", statement.table))
                        })
                })
                .collect::<Result<_, _>>()?
        };
        let mut inserted = 0usize;
        for row_values in &statement.rows {
            if row_values.len() != positions.len() {
                return Err(FakeDbError(format!(
                    "INSERT row has {} values but {} columns were targeted",
                    row_values.len(),
                    positions.len()
                )));
            }
            let mut row = vec![Value::Null; table.columns.len()];
            for (value, &position) in row_values.iter().zip(&positions) {
                row[position] = value.clone();
            }
            table.rows.push(row);
            inserted += 1;
        }
        execution
            .messages
            .push(format!("({inserted} rows affected)"));
        Ok(())
    }

    fn run_update(&mut self, sql: &str, execution: &mut Execution) -> Result<(), FakeDbError> {
        let statement = parser::parse_update(sql)?;
        let table = self
            .tables
            .get_mut(&statement.table.to_lowercase())
            .ok_or_else(|| FakeDbError(format!("Unknown table '{}'", statement.table)))?;
        let assignments: Vec<(usize, Value)> = statement
            .assignments
            .iter()
            .map(|(name, value)| {
                table
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(name))
                    .map(|position| (position, value.clone()))
                    .ok_or_else(|| {
                        FakeDbError(format!("Unknown column '{name}' on '{}'", statement.table))
                    })
            })
            .collect::<Result<_, _>>()?;
        let filter_position = match &statement.where_clause {
            Some(condition) => Some(
                table
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&condition.column))
                    .ok_or_else(|| {
                        FakeDbError(format!(
                            "Unknown column '{}' on '{}'",
                            condition.column, statement.table
                        ))
                    })?,
            ),
            None => None,
        };
        let mut updated = 0usize;
        for row in &mut table.rows {
            let matches = match (&statement.where_clause, filter_position) {
                (Some(condition), Some(position)) => row
                    .get(position)
                    .is_some_and(|cell| condition.matches(cell)),
                _ => true,
            };
            if matches {
                for (position, value) in &assignments {
                    row[*position] = value.clone();
                }
                updated += 1;
            }
        }
        execution
            .messages
            .push(format!("({updated} rows affected)"));
        Ok(())
    }

    fn run_delete(&mut self, sql: &str, execution: &mut Execution) -> Result<(), FakeDbError> {
        let statement = parser::parse_delete(sql)?;
        let table = self
            .tables
            .get_mut(&statement.table.to_lowercase())
            .ok_or_else(|| FakeDbError(format!("Unknown table '{}'", statement.table)))?;
        let before = table.rows.len();
        match &statement.where_clause {
            Some(condition) => {
                let position = table
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&condition.column))
                    .ok_or_else(|| {
                        FakeDbError(format!(
                            "Unknown column '{}' on '{}'",
                            condition.column, statement.table
                        ))
                    })?;
                table.rows.retain(|row| {
                    !row.get(position)
                        .is_some_and(|cell| condition.matches(cell))
                });
            }
            None => table.rows.clear(),
        }
        execution
            .messages
            .push(format!("({} rows affected)", before - table.rows.len()));
        Ok(())
    }

    fn run_create_table(
        &mut self,
        sql: &str,
        execution: &mut Execution,
    ) -> Result<(), FakeDbError> {
        let statement = parser::parse_create_table(sql)?;
        if self.tables.contains_key(&statement.table.to_lowercase()) {
            return Err(FakeDbError(format!(
                "There is already an object named '{}'",
                statement.table
            )));
        }
        self.add_table(Table {
            name: statement.table,
            columns: statement
                .columns
                .iter()
                .map(|(name, type_name)| Column::new(name, type_name))
                .collect(),
            rows: Vec::new(),
        });
        execution
            .messages
            .push("Commands completed successfully.".to_string());
        Ok(())
    }

    fn run_drop_table(&mut self, sql: &str, execution: &mut Execution) -> Result<(), FakeDbError> {
        let mut words = sql.split_whitespace();
        let (Some(drop), Some(kind), Some(name)) = (words.next(), words.next(), words.next())
        else {
            return Err(FakeDbError("Expected DROP TABLE <name>".to_string()));
        };
        if !drop.eq_ignore_ascii_case("DROP") || !kind.eq_ignore_ascii_case("TABLE") {
            return Err(FakeDbError(
                "Only DROP TABLE is supported by the demo database".to_string(),
            ));
        }
        let key = name
            .trim_matches(|c| c == '[' || c == ']' || c == ';')
            .to_lowercase();
        if self.tables.remove(&key).is_none() {
            return Err(FakeDbError(format!("Unknown table '{name}'")));
        }
        execution
            .messages
            .push("Commands completed successfully.".to_string());
        Ok(())
    }

    /// Execute a SQL batch. Statements are separated by `;` or the `GO`
    /// batch separator on its own line; each SELECT/EXEC contributes one
    /// result set.
    pub fn execute(&self, sql: &str) -> Result<Execution, FakeDbError> {
        let mut execution = Execution {
            result_sets: Vec::new(),
            messages: Vec::new(),
        };
        let statements = parser::split_statements(sql);
        if statements.is_empty() {
            execution
                .messages
                .push("Commands completed successfully.".to_string());
            return Ok(execution);
        }
        for statement in statements {
            self.execute_statement(&statement, &mut execution)?;
        }
        Ok(execution)
    }

    fn execute_statement(
        &self,
        statement: &str,
        execution: &mut Execution,
    ) -> Result<(), FakeDbError> {
        let trimmed = statement.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        let first_word = trimmed
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_uppercase();
        match first_word.as_str() {
            "SELECT" => {
                let rs = self.run_select(trimmed)?;
                execution
                    .messages
                    .push(format!("({} rows affected)", rs.row_count()));
                execution.result_sets.push(rs);
                Ok(())
            }
            "EXEC" | "EXECUTE" => {
                let (name, args) = parser::parse_exec(trimmed)?;
                let proc = self.procedure(&name).ok_or_else(|| {
                    FakeDbError(format!(
                        "Could not find stored procedure '{name}'. \
                         Available: {}",
                        self.procedures
                            .values()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
                let rs = self.run_procedure(&proc.name.clone(), &args)?;
                execution
                    .messages
                    .push(format!("({} rows affected)", rs.row_count()));
                execution.result_sets.push(rs);
                Ok(())
            }
            "INSERT" | "UPDATE" | "DELETE" | "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "MERGE" => {
                Err(FakeDbError(format!(
                    "{first_word} is not supported by the demo database — it is read-only. \
                 Load a local SQLite file to run modifying statements."
                )))
            }
            _ => Err(FakeDbError(format!(
                "Statement '{first_word}' is not supported by the demo database. \
                 Supported: SELECT (with WHERE / ORDER BY / TOP / LIMIT), EXEC <procedure>."
            ))),
        }
    }

    /// Execute one `SELECT ...`, resolving views.
    fn run_select(&self, sql: &str) -> Result<ResultSet, FakeDbError> {
        let query = parser::parse_select(sql)?;

        // Resolve the source: a table, or a view (recursively execute its
        // definition, then apply this query's filters over the result).
        let source: ResultSet = if let Some(table) = self.table(&query.from) {
            ResultSet {
                columns: table.columns.clone(),
                rows: table.rows.clone(),
            }
        } else if let Some(view) = self.view(&query.from) {
            self.run_select(&view.definition.clone())?
        } else {
            return Err(FakeDbError(format!(
                "Invalid object name '{}'. Available tables: {}; views: {}",
                query.from,
                self.tables
                    .values()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                self.views
                    .values()
                    .map(|v| v.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };

        let col_index = |name: &str| -> Result<usize, FakeDbError> {
            source
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| FakeDbError(format!("Invalid column name '{name}'.")))
        };

        // WHERE
        let mut rows: Vec<Vec<Value>> = if let Some(cond) = &query.where_clause {
            let idx = col_index(&cond.column)?;
            source
                .rows
                .iter()
                .filter(|row| cond.matches(&row[idx]))
                .cloned()
                .collect()
        } else {
            source.rows.clone()
        };

        // ORDER BY
        if let Some(order) = &query.order_by {
            let idx = col_index(&order.column)?;
            rows.sort_by(|a, b| {
                let ord = a[idx]
                    .sort_key()
                    .partial_cmp(&b[idx].sort_key())
                    .unwrap_or(std::cmp::Ordering::Equal);
                if order.descending { ord.reverse() } else { ord }
            });
        }

        // TOP / LIMIT
        if let Some(limit) = query.limit {
            rows.truncate(limit);
        }

        // Projection
        let (columns, rows) = match &query.projection {
            parser::Projection::All => (source.columns.clone(), rows),
            parser::Projection::Columns(names) => {
                let indices = names
                    .iter()
                    .map(|n| col_index(n))
                    .collect::<Result<Vec<_>, _>>()?;
                let columns = indices.iter().map(|&i| source.columns[i].clone()).collect();
                let rows = rows
                    .into_iter()
                    .map(|row| indices.iter().map(|&i| row[i].clone()).collect())
                    .collect();
                (columns, rows)
            }
            parser::Projection::CountStar => {
                let count = rows.len() as i64;
                (
                    vec![Column::new("count", "int")],
                    vec![vec![Value::Int(count)]],
                )
            }
        };

        Ok(ResultSet { columns, rows })
    }

    /// Dispatch a stored procedure by name.
    fn run_procedure(&self, name: &str, args: &[Value]) -> Result<ResultSet, FakeDbError> {
        demo::run_demo_procedure(self, name, args)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn insert_update_delete_round_trip() {
        let mut db = demo_database();
        let before = db
            .execute("SELECT COUNT(*) FROM Customers")
            .unwrap()
            .result_sets[0]
            .rows[0][0]
            .clone();

        let exec = db
            .execute_mut("INSERT INTO Customers (CustomerId, CompanyName) VALUES (999, 'Acme')")
            .unwrap();
        assert!(exec.messages.iter().any(|m| m.contains("1 rows affected")));

        let after = db
            .execute("SELECT COUNT(*) FROM Customers")
            .unwrap()
            .result_sets[0]
            .rows[0][0]
            .clone();
        match (before, after) {
            (Value::Int(b), Value::Int(a)) => assert_eq!(a, b + 1),
            other => panic!("count values: {other:?}"),
        }

        let exec = db
            .execute_mut("UPDATE Customers SET CompanyName = 'Acme 2' WHERE CustomerId = 999")
            .unwrap();
        assert!(exec.messages.iter().any(|m| m.contains("1 rows affected")));
        let read = db
            .execute("SELECT CompanyName FROM Customers WHERE CustomerId = 999")
            .unwrap();
        assert_eq!(read.result_sets[0].rows[0][0], Value::Text("Acme 2".into()));

        let exec = db
            .execute_mut("DELETE FROM Customers WHERE CustomerId = 999")
            .unwrap();
        assert!(exec.messages.iter().any(|m| m.contains("1 rows affected")));
    }

    #[test]
    fn create_insert_select_drop_table() {
        let mut db = FakeDatabase::new();
        db.execute_mut("CREATE TABLE notes (id INT, body NVARCHAR(100))")
            .unwrap();
        db.execute_mut("INSERT INTO notes VALUES (1, 'hello'), (2, 'world')")
            .unwrap();
        let read = db
            .execute("SELECT body FROM notes ORDER BY id DESC")
            .unwrap();
        assert_eq!(read.result_sets[0].rows.len(), 2);
        assert_eq!(read.result_sets[0].rows[0][0], Value::Text("world".into()));
        // Creating it again is an honest error.
        assert!(db.execute_mut("CREATE TABLE notes (id INT)").is_err());
        db.execute_mut("DROP TABLE notes").unwrap();
        assert!(db.execute("SELECT * FROM notes").is_err());
    }

    #[test]
    fn read_only_execute_still_rejects_mutations() {
        let db = demo_database();
        let err = db.execute("DELETE FROM Customers").unwrap_err();
        assert!(err.0.contains("read-only"), "{err}");
    }

    #[test]
    fn mutation_errors_are_actionable() {
        let mut db = demo_database();
        assert!(db.execute_mut("INSERT INTO nope VALUES (1)").is_err());
        assert!(
            db.execute_mut("INSERT INTO Customers (Bogus) VALUES (1)")
                .is_err()
        );
        assert!(db.execute_mut("UPDATE Customers SET Bogus = 1").is_err());
    }

    use super::*;

    fn db() -> FakeDatabase {
        demo_database()
    }

    #[test]
    fn demo_database_has_tables_views_and_procedures() {
        let schema = db().schema();
        assert!(schema.tables.len() >= 4, "want several tables");
        assert!(!schema.views.is_empty(), "want at least one view");
        assert!(
            schema.procedures.len() >= 2,
            "want at least two stored procedures"
        );
        // View columns resolve through their definitions.
        assert!(schema.views.iter().all(|v| !v.columns.is_empty()));
    }

    #[test]
    fn select_star_returns_all_rows() {
        let out = db().execute("SELECT * FROM Customers").unwrap();
        assert_eq!(out.result_sets.len(), 1);
        let rs = &out.result_sets[0];
        assert!(rs.row_count() >= 5);
        assert!(rs.columns.iter().any(|c| c.name == "CompanyName"));
    }

    #[test]
    fn select_projection_and_where() {
        let out = db()
            .execute("SELECT ProductName, UnitPrice FROM Products WHERE UnitPrice > 20")
            .unwrap();
        let rs = &out.result_sets[0];
        assert_eq!(rs.columns.len(), 2);
        assert!(rs.rows.iter().all(|r| match &r[1] {
            Value::Float(f) => *f > 20.0,
            Value::Int(i) => *i > 20,
            other => panic!("unexpected value {other:?}"),
        }));
    }

    #[test]
    fn select_order_by_desc_and_top() {
        let out = db()
            .execute("SELECT TOP 3 * FROM Products ORDER BY UnitPrice DESC")
            .unwrap();
        let rs = &out.result_sets[0];
        assert_eq!(rs.row_count(), 3);
        let price = |row: &Vec<Value>| match &row[rs
            .columns
            .iter()
            .position(|c| c.name == "UnitPrice")
            .unwrap()]
        {
            Value::Float(f) => *f,
            Value::Int(i) => *i as f64,
            other => panic!("unexpected {other:?}"),
        };
        assert!(price(&rs.rows[0]) >= price(&rs.rows[1]));
        assert!(price(&rs.rows[1]) >= price(&rs.rows[2]));
    }

    #[test]
    fn limit_clause_works_like_top() {
        let out = db().execute("SELECT * FROM Customers LIMIT 2").unwrap();
        assert_eq!(out.result_sets[0].row_count(), 2);
    }

    #[test]
    fn select_from_view_resolves_definition() {
        let out = db().execute("SELECT * FROM ExpensiveProducts").unwrap();
        let rs = &out.result_sets[0];
        assert!(rs.row_count() >= 1);
        assert!(rs.columns.iter().any(|c| c.name == "ProductName"));
    }

    #[test]
    fn where_on_view_applies_after_view_resolution() {
        let all = db().execute("SELECT * FROM ExpensiveProducts").unwrap();
        let filtered = db()
            .execute("SELECT * FROM ExpensiveProducts WHERE CategoryName = 'Beverages'")
            .unwrap();
        assert!(
            filtered.result_sets[0].row_count() <= all.result_sets[0].row_count(),
            "WHERE over a view must filter its resolved rows"
        );
    }

    #[test]
    fn exec_procedure_returns_result_set() {
        let out = db().execute("EXEC TopProductsByPrice 2").unwrap();
        assert_eq!(out.result_sets[0].row_count(), 2);
    }

    #[test]
    fn exec_customer_orders_with_text_arg() {
        let out = db().execute("EXEC CustomerOrders 'ALFKI'").unwrap();
        assert!(out.result_sets[0].row_count() >= 1);
    }

    #[test]
    fn unknown_table_reports_available_objects() {
        let err = db().execute("SELECT * FROM NoSuchTable").unwrap_err();
        assert!(err.0.contains("Invalid object name"));
        assert!(err.0.contains("Customers"));
    }

    #[test]
    fn unknown_procedure_reports_available() {
        let err = db().execute("EXEC NoSuchProc").unwrap_err();
        assert!(err.0.contains("Could not find stored procedure"));
    }

    #[test]
    fn writes_are_rejected_read_only() {
        let err = db().execute("DELETE FROM Customers").unwrap_err();
        assert!(err.0.contains("read-only"));
    }

    #[test]
    fn multi_statement_batches_yield_multiple_result_sets() {
        let out = db()
            .execute("SELECT * FROM Customers; SELECT * FROM Products")
            .unwrap();
        assert_eq!(out.result_sets.len(), 2);
    }

    #[test]
    fn go_separator_splits_batches() {
        let out = db()
            .execute("SELECT * FROM Customers\nGO\nSELECT * FROM Products")
            .unwrap();
        assert_eq!(out.result_sets.len(), 2);
    }

    #[test]
    fn count_star_supported() {
        let out = db().execute("SELECT COUNT(*) FROM Customers").unwrap();
        let rs = &out.result_sets[0];
        assert_eq!(rs.row_count(), 1);
        assert!(matches!(rs.rows[0][0], Value::Int(n) if n >= 5));
    }

    #[test]
    fn null_sorts_first_ascending() {
        let mut fake = FakeDatabase::new();
        fake.add_table(Table {
            name: "T".into(),
            columns: vec![Column::new("a", "int")],
            rows: vec![vec![Value::Int(2)], vec![Value::Null], vec![Value::Int(1)]],
        });
        let out = fake.execute("SELECT * FROM T ORDER BY a").unwrap();
        assert_eq!(out.result_sets[0].rows[0][0], Value::Null);
    }
}
