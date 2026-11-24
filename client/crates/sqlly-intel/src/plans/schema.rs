//! Schema context for plan findings.
//!
//! A [`SchemaContext`] is plain data describing the tables a plan touches:
//! columns with their declared types and nullability, primary keys, unique
//! constraints, foreign keys, indexes (key columns, included columns, partial
//! / filtered predicates) and, when known, row counts.
//!
//! It is built two ways:
//!
//! - [`SchemaContext::from_ddl`] reads pasted DDL (`CREATE TABLE`,
//!   `CREATE INDEX`, `ALTER TABLE … ADD …`) in any supported dialect. The
//!   reader is tolerant: a statement it cannot read becomes a
//!   [`SchemaNote`] and the rest of the script is still used. Lines of the
//!   form `table: 1200000` (optionally `1.2M`, `250k`, `-- orders: 5000 rows`)
//!   supply row counts.
//! - [`SchemaContext::from_tables`] / [`SchemaTable::from_catalog`] wrap
//!   metadata the desktop app already holds in memory.
//!
//! The findings engine only ever reads this struct; it never talks to a
//! database.

use crate::plans::dialects::PlanDialect;
use sqlly_core::DisplayMessage;
use sqlly_core::object_model::{ColumnInfo, ConstraintInfo, IndexInfo};
use sqlly_lang::sqllex::{IdentQuote, StrictLexer, Token, TokenKind};
use sqlly_lang::sqlmodel::Dialect;

/// Everything the findings engine knows about the schema behind a plan.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SchemaContext {
    /// Dialect the DDL was read as, when built from DDL.
    pub dialect: Option<PlanDialect>,
    pub tables: Vec<SchemaTable>,
    /// Statements that looked like schema DDL but could not be read.
    pub notes: Vec<SchemaNote>,
    /// Statements skipped because they do not describe tables or indexes
    /// (`SET`, `INSERT`, `CREATE VIEW`, …). Not an error.
    pub ignored_statements: usize,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SchemaTable {
    /// Schema / database qualifier as written; empty when unqualified.
    pub schema: String,
    pub name: String,
    /// Empty when only an index or a row count named the table.
    pub columns: Vec<SchemaColumn>,
    /// Primary key, when one is declared.
    pub primary_key: Option<SchemaIndex>,
    /// Secondary indexes and unique constraints (not the primary key).
    pub indexes: Vec<SchemaIndex>,
    pub foreign_keys: Vec<SchemaForeignKey>,
    pub row_count: Option<f64>,
    /// `PARTITION BY …` text, when the table is partitioned.
    pub partition_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SchemaColumn {
    pub name: String,
    /// Declared type as written, whitespace-normalized (`varchar(200)`,
    /// `timestamp with time zone`, `Nullable(String)`). Empty when the DDL
    /// gives none (SQLite allows that).
    pub type_name: String,
    pub nullable: bool,
    pub collation: Option<String>,
    /// Identity / auto-increment / serial column.
    pub identity: bool,
    /// Computed / generated column.
    pub computed: bool,
}

/// One key column of an index.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SchemaIndexColumn {
    /// Column name, or the expression text for an expression key.
    pub name: String,
    pub descending: bool,
    /// The key is an expression (`lower(email)`), not a plain column.
    pub expression: bool,
    /// MySQL prefix length (`name(20)`).
    pub prefix_length: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SchemaIndex {
    /// Empty for an unnamed constraint.
    pub name: String,
    pub columns: Vec<SchemaIndexColumn>,
    /// Non-key columns stored in the index (`INCLUDE (…)`).
    pub include_columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    /// `Some(true)` for a clustered index, `Some(false)` for an explicitly
    /// nonclustered one, `None` when the DDL does not say.
    pub clustered: Option<bool>,
    /// Partial / filtered index predicate (`WHERE …`), without the keyword.
    pub filter: Option<String>,
    /// Access method when stated (`btree`, `gin`, `hash`, `columnstore`, …),
    /// lowercased.
    pub method: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SchemaForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_schema: String,
    pub referenced_table: String,
    /// Empty when the DDL references the parent's primary key implicitly.
    pub referenced_columns: Vec<String>,
}

/// Why a statement could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaNoteReason {
    /// `CREATE TABLE` without a readable column list (`AS SELECT`, `LIKE`,
    /// `PARTITION OF`, or cut off).
    TableWithoutColumns,
    /// `CREATE INDEX` whose table or key list could not be read.
    MalformedIndex,
    /// `ALTER TABLE` whose table or action could not be read.
    MalformedAlter,
    /// A table element (column or constraint) that could not be read; the
    /// rest of the table was kept.
    MalformedElement,
    /// An index kind the findings engine does not reason about (full-text,
    /// spatial, ClickHouse data-skipping indexes).
    UnsupportedIndexKind,
}

/// A piece of the pasted DDL that was not used.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaNote {
    /// 1-based line where the statement starts.
    pub line: usize,
    /// The start of the statement, trimmed to one short line.
    pub snippet: String,
    pub reason: SchemaNoteReason,
}

impl SchemaNote {
    /// User-facing explanation of the note.
    pub fn message(&self) -> DisplayMessage {
        let (line, snippet) = (self.line, self.snippet.as_str());
        let (key, fallback) = match self.reason {
            SchemaNoteReason::TableWithoutColumns => (
                "services.plans.schema.note.table_without_columns",
                format!(
                    "Line {line}: no column list could be read for this table, so its columns are unknown: {snippet}"
                ),
            ),
            SchemaNoteReason::MalformedIndex => (
                "services.plans.schema.note.malformed_index",
                format!(
                    "Line {line}: this index definition could not be read and was skipped: {snippet}"
                ),
            ),
            SchemaNoteReason::MalformedAlter => (
                "services.plans.schema.note.malformed_alter",
                format!(
                    "Line {line}: this ALTER TABLE could not be read and was skipped: {snippet}"
                ),
            ),
            SchemaNoteReason::MalformedElement => (
                "services.plans.schema.note.malformed_element",
                format!(
                    "Line {line}: this column or constraint could not be read and was skipped: {snippet}"
                ),
            ),
            SchemaNoteReason::UnsupportedIndexKind => (
                "services.plans.schema.note.unsupported_index_kind",
                format!("Line {line}: this kind of index is not used for plan advice: {snippet}"),
            ),
        };
        DisplayMessage::new(key, fallback)
            .with_arg("line", line.to_string())
            .with_arg("snippet", snippet)
    }
}

/// Coarse type family used to compare the two sides of a join or predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeFamily {
    Integer,
    Decimal,
    Float,
    /// Character data; `unicode` is true for `nvarchar` / `nchar` / `ntext`.
    Text {
        unicode: bool,
    },
    Temporal,
    Boolean,
    Uuid,
    Binary,
    Json,
    Other,
}

impl TypeFamily {
    /// Classifies a declared type name. Unknown names are [`TypeFamily::Other`].
    pub fn of(type_name: &str) -> Self {
        let mut lower = type_name.trim().to_ascii_lowercase();
        // ClickHouse wrappers do not change the family.
        for wrapper in ["nullable(", "lowcardinality("] {
            while let Some(rest) = lower.strip_prefix(wrapper) {
                lower = rest.trim_end_matches(')').to_string();
            }
        }
        let base: String = lower
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ' ')
            .collect();
        let base = base.trim();
        let first = base.split(' ').next().unwrap_or("");
        match first {
            "int" | "integer" | "bigint" | "smallint" | "tinyint" | "mediumint" | "int2"
            | "int4" | "int8" | "serial" | "bigserial" | "smallserial" | "hugeint" | "int16"
            | "int32" | "int64" | "int128" | "int256" | "uint8" | "uint16" | "uint32"
            | "uint64" | "uint128" | "uint256" | "ubigint" | "uinteger" | "usmallint"
            | "utinyint" | "year" => Self::Integer,
            "numeric" | "decimal" | "dec" | "money" | "smallmoney" | "number" | "decimal32"
            | "decimal64" | "decimal128" | "decimal256" => Self::Decimal,
            "real" | "float" | "double" | "float4" | "float8" | "float32" | "float64"
            | "binary_float" | "binary_double" => Self::Float,
            "nvarchar" | "nchar" | "ntext" | "nvarchar2" | "nclob" => Self::Text { unicode: true },
            "varchar" | "char" | "character" | "text" | "tinytext" | "mediumtext" | "longtext"
            | "varchar2" | "clob" | "string" | "fixedstring" | "bpchar" | "citext" | "name"
            | "sysname" | "enum" | "set" => Self::Text { unicode: false },
            "date" | "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" | "time"
            | "timetz" | "timestamp" | "timestamptz" | "interval" | "date32" | "datetime64" => {
                Self::Temporal
            }
            "bool" | "boolean" | "bit" => Self::Boolean,
            "uuid" | "uniqueidentifier" => Self::Uuid,
            "binary" | "varbinary" | "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob"
            | "raw" | "image" | "rowversion" => Self::Binary,
            "json" | "jsonb" => Self::Json,
            _ => Self::Other,
        }
    }
}

impl SchemaColumn {
    pub fn type_family(&self) -> TypeFamily {
        TypeFamily::of(&self.type_name)
    }
}

impl SchemaIndex {
    /// Key column names in order (expression keys included as their text).
    pub fn key_names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| c.name.as_str()).collect()
    }

    /// Every column the index stores: keys, then included columns.
    pub fn stored_columns(&self) -> impl Iterator<Item = &str> {
        self.columns
            .iter()
            .filter(|c| !c.expression)
            .map(|c| c.name.as_str())
            .chain(self.include_columns.iter().map(String::as_str))
    }

    /// True when the index is an ordered b-tree style index a range seek or
    /// an ordered read can use (not hash, GIN, full-text, columnstore, …).
    pub fn is_ordered(&self) -> bool {
        match self.method.as_deref() {
            None => true,
            Some(method) => matches!(method, "btree" | "b-tree" | "clustered" | "nonclustered"),
        }
    }
}

impl SchemaTable {
    /// `schema.name`, or just `name` when unqualified.
    pub fn qualified_name(&self) -> String {
        if self.schema.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.schema, self.name)
        }
    }

    pub fn column(&self, name: &str) -> Option<&SchemaColumn> {
        let name = bare_name(name);
        self.columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(&name))
    }

    /// True when column definitions are known (not just an index or row
    /// count that mentioned the table).
    pub fn has_columns(&self) -> bool {
        !self.columns.is_empty()
    }

    /// The primary key (first, when present) followed by every other index.
    pub fn all_indexes(&self) -> impl Iterator<Item = &SchemaIndex> {
        self.primary_key.iter().chain(self.indexes.iter())
    }

    pub fn index(&self, name: &str) -> Option<&SchemaIndex> {
        let name = bare_name(name);
        self.all_indexes()
            .find(|index| !index.name.is_empty() && index.name.eq_ignore_ascii_case(&name))
    }

    /// Builds a table from the catalog metadata the desktop app holds.
    /// `constraints` supplies foreign keys when their `definition` carries a
    /// `REFERENCES` clause; primary keys and unique constraints come from
    /// `indexes`.
    pub fn from_catalog(
        dialect: PlanDialect,
        schema: &str,
        name: &str,
        columns: &[ColumnInfo],
        indexes: &[IndexInfo],
        constraints: &[ConstraintInfo],
        row_count: Option<f64>,
    ) -> Self {
        let mut table = SchemaTable {
            schema: schema.to_string(),
            name: name.to_string(),
            row_count,
            ..Default::default()
        };
        let mut ordered: Vec<&ColumnInfo> = columns.iter().collect();
        ordered.sort_by_key(|column| column.ordinal);
        for column in ordered {
            table.columns.push(SchemaColumn {
                name: column.name.clone(),
                type_name: catalog_type_name(column),
                nullable: column.is_nullable,
                collation: column.collation.clone().filter(|c| !c.is_empty()),
                identity: column.is_identity,
                computed: column.is_computed,
            });
        }
        for index in indexes {
            let type_desc = index.type_desc.to_ascii_lowercase();
            if type_desc == "heap" || index.columns.is_empty() {
                continue;
            }
            let clustered = if type_desc.contains("nonclustered") {
                Some(false)
            } else if type_desc.contains("clustered") {
                Some(true)
            } else {
                None
            };
            let method = if type_desc.contains("columnstore") {
                Some("columnstore".to_string())
            } else if type_desc.is_empty() || type_desc.contains("clustered") {
                None
            } else {
                Some(type_desc.clone())
            };
            let built = SchemaIndex {
                name: index.name.clone(),
                columns: index
                    .columns
                    .iter()
                    .map(|column| catalog_index_column(column))
                    .collect(),
                include_columns: index.included_columns.clone(),
                unique: index.is_unique || index.is_primary_key,
                primary: index.is_primary_key,
                clustered,
                filter: index
                    .filter_definition
                    .clone()
                    .filter(|filter| !filter.trim().is_empty()),
                method,
            };
            if built.primary && table.primary_key.is_none() {
                table.primary_key = Some(built);
            } else {
                table.indexes.push(built);
            }
        }
        for constraint in constraints {
            if !constraint
                .type_desc
                .to_ascii_lowercase()
                .contains("foreign")
            {
                continue;
            }
            let Some(definition) = constraint.definition.as_deref() else {
                continue;
            };
            let lex = lex_dialect(dialect);
            let tokens = significant_tokens(definition, lex);
            let mut cursor = Cursor::new(&tokens);
            while !cursor.done() && !cursor.at_word("REFERENCES") {
                cursor.bump();
            }
            if let Some((referenced_schema, referenced_table, referenced_columns)) =
                parse_references(&mut cursor, dialect)
            {
                table.foreign_keys.push(SchemaForeignKey {
                    name: constraint.name.clone(),
                    columns: constraint.columns.clone(),
                    referenced_schema,
                    referenced_table,
                    referenced_columns,
                });
            }
        }
        table
    }
}

fn catalog_type_name(column: &ColumnInfo) -> String {
    let base = column.type_name.trim();
    if base.contains('(') {
        return base.to_string();
    }
    let lower = base.to_ascii_lowercase();
    match lower.as_str() {
        "varchar" | "char" | "varbinary" | "binary" | "nvarchar" | "nchar" => {
            if column.max_length < 0 {
                format!("{base}(max)")
            } else if column.max_length == 0 {
                base.to_string()
            } else if lower.starts_with('n') {
                // SQL Server reports nvarchar/nchar length in bytes.
                format!("{base}({})", column.max_length / 2)
            } else {
                format!("{base}({})", column.max_length)
            }
        }
        "decimal" | "numeric" if column.precision > 0 => {
            format!("{base}({},{})", column.precision, column.scale)
        }
        _ => base.to_string(),
    }
}

fn catalog_index_column(raw: &str) -> SchemaIndexColumn {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    let (name, descending) = if let Some(stripped) = lower.strip_suffix(" desc") {
        (&trimmed[..stripped.len()], true)
    } else if let Some(stripped) = lower.strip_suffix(" asc") {
        (&trimmed[..stripped.len()], false)
    } else {
        (trimmed, false)
    };
    SchemaIndexColumn {
        name: bare_name(name),
        descending,
        expression: name.contains('('),
        prefix_length: None,
    }
}

impl SchemaContext {
    /// Wraps tables built from catalog metadata.
    pub fn from_tables(dialect: PlanDialect, tables: Vec<SchemaTable>) -> Self {
        Self {
            dialect: Some(dialect),
            tables,
            notes: Vec::new(),
            ignored_statements: 0,
        }
    }

    /// True when nothing usable was read.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Finds a table by name. Matching ignores case and quoting. When
    /// `schema` is given and the stored table has a schema, both must match,
    /// unless exactly one table carries that name.
    pub fn table(&self, schema: Option<&str>, name: &str) -> Option<&SchemaTable> {
        let name = bare_name(name);
        let schema = schema.map(bare_name).filter(|s| !s.is_empty());
        let candidates: Vec<&SchemaTable> = self
            .tables
            .iter()
            .filter(|table| table.name.eq_ignore_ascii_case(&name))
            .collect();
        if candidates.len() <= 1 {
            return candidates.first().copied();
        }
        match schema {
            Some(schema) => candidates
                .iter()
                .find(|table| table.schema.eq_ignore_ascii_case(&schema))
                .or_else(|| candidates.iter().find(|table| table.schema.is_empty()))
                .copied(),
            None => candidates
                .iter()
                .find(|table| table.schema.is_empty())
                .or(candidates.first())
                .copied(),
        }
    }

    /// Reads schema DDL. Never fails: unreadable statements are reported in
    /// [`SchemaContext::notes`], everything else is kept.
    pub fn from_ddl(text: &str, dialect: PlanDialect) -> Self {
        let mut context = SchemaContext {
            dialect: Some(dialect),
            ..Default::default()
        };
        let (sql, row_counts) = extract_row_counts(text);
        let lex = lex_dialect(dialect);
        let tokens = significant_tokens(&sql, lex);
        for statement in split_statements(&tokens, dialect) {
            DdlReader {
                src: &sql,
                dialect,
                context: &mut context,
            }
            .statement(statement);
        }
        for (schema, name, rows) in row_counts {
            let schema = fold_name(dialect, &schema, false);
            let name = fold_name(dialect, &name, false);
            context.table_mut(&schema, &name).row_count = Some(rows);
        }
        context
    }

    fn table_mut(&mut self, schema: &str, name: &str) -> &mut SchemaTable {
        let position = self.tables.iter().position(|table| {
            table.name.eq_ignore_ascii_case(name)
                && (schema.is_empty()
                    || table.schema.is_empty()
                    || table.schema.eq_ignore_ascii_case(schema))
        });
        match position {
            Some(position) => {
                let table = &mut self.tables[position];
                if table.schema.is_empty() && !schema.is_empty() {
                    table.schema = schema.to_string();
                }
                table
            }
            None => {
                self.tables.push(SchemaTable {
                    schema: schema.to_string(),
                    name: name.to_string(),
                    ..Default::default()
                });
                self.tables.last_mut().expect("just pushed")
            }
        }
    }
}

/// Strips one level of identifier quoting and any qualifier, leaving the
/// last name part (`[dbo].[Orders]` → `Orders`).
pub(crate) fn bare_name(raw: &str) -> String {
    split_qualified(raw).pop().unwrap_or_default()
}

/// Splits `a.b.c` into parts, honouring `[..]`, `".."` and `` `..` `` quoting
/// and stripping the quotes.
pub(crate) fn split_qualified(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut close: Option<char> = None;
    for ch in raw.trim().chars() {
        match close {
            Some(end) if ch == end => close = None,
            Some(_) => current.push(ch),
            None => match ch {
                '[' => close = Some(']'),
                '"' => close = Some('"'),
                '`' => close = Some('`'),
                '.' => parts.push(std::mem::take(&mut current)),
                _ => current.push(ch),
            },
        }
    }
    parts.push(current);
    parts.retain(|part| !part.is_empty());
    parts
}

fn lex_dialect(dialect: PlanDialect) -> Dialect {
    match dialect {
        PlanDialect::SqlServer => Dialect::SqlServer,
        PlanDialect::Postgres | PlanDialect::DuckDb => Dialect::Postgres,
        PlanDialect::MySql | PlanDialect::MariaDb | PlanDialect::ClickHouse => Dialect::MySql,
        PlanDialect::Sqlite => Dialect::Sqlite,
        PlanDialect::Oracle => Dialect::Oracle,
    }
}

/// Applies the engine's folding of unquoted identifiers so names read from
/// DDL match the names a plan reports.
fn fold_name(dialect: PlanDialect, name: &str, quoted: bool) -> String {
    if quoted {
        return name.to_string();
    }
    match dialect {
        PlanDialect::Postgres | PlanDialect::DuckDb => name.to_ascii_lowercase(),
        PlanDialect::Oracle => name.to_ascii_uppercase(),
        _ => name.to_string(),
    }
}

fn significant_tokens(src: &str, dialect: Dialect) -> Vec<Token<'_>> {
    StrictLexer::tokenize(src, dialect)
        .into_iter()
        .filter(|token| !token.is_trivia())
        .collect()
}

fn is_word(token: &Token<'_>, word: &str) -> bool {
    token.is_word() && !token.ident_quoted() && token.text.eq_ignore_ascii_case(word)
}

fn is_any_word(token: &Token<'_>, words: &[&str]) -> bool {
    words.iter().any(|word| is_word(token, word))
}

/// Pulls `table: rows` lines out of the text. Returns the text with those
/// lines blanked (so line numbers stay right) and the parsed counts.
fn extract_row_counts(text: &str) -> (String, Vec<(String, String, f64)>) {
    let mut sql = String::with_capacity(text.len());
    let mut counts = Vec::new();
    for line in text.split_inclusive('\n') {
        match parse_row_count_line(line) {
            Some(count) => {
                counts.push(count);
                if line.ends_with('\n') {
                    sql.push('\n');
                }
            }
            None => sql.push_str(line),
        }
    }
    (sql, counts)
}

fn parse_row_count_line(line: &str) -> Option<(String, String, f64)> {
    let mut body = line.trim();
    for prefix in ["--", "#", "//"] {
        if let Some(rest) = body.strip_prefix(prefix) {
            body = rest.trim();
            break;
        }
    }
    let body = body.trim_end_matches([';', ',']).trim();
    let (name, value) = body.rsplit_once(':')?;
    let name = name.trim();
    let quoted = name.contains(['"', '`', '[']);
    if name.is_empty()
        || !name.chars().all(|c| {
            c.is_alphanumeric()
                || matches!(c, '_' | '.' | '"' | '`' | '[' | ']' | '$')
                || (quoted && c == ' ')
        })
    {
        return None;
    }
    let rows = parse_row_count_value(value)?;
    let mut parts = split_qualified(name);
    let table = parts.pop()?;
    let schema = parts.pop().unwrap_or_default();
    Some((schema, table, rows))
}

fn parse_row_count_value(raw: &str) -> Option<f64> {
    let mut value = raw.trim().to_ascii_lowercase();
    for suffix in [" rows", " row"] {
        if let Some(rest) = value.strip_suffix(suffix) {
            value = rest.trim().to_string();
        }
    }
    let value = value.trim_start_matches('~');
    let (digits, multiplier) = match value.chars().last()? {
        'k' => (&value[..value.len() - 1], 1e3),
        'm' => (&value[..value.len() - 1], 1e6),
        'b' | 'g' => (&value[..value.len() - 1], 1e9),
        _ => (value, 1.0),
    };
    let digits = digits.trim();
    if digits.is_empty()
        || !digits.starts_with(|c: char| c.is_ascii_digit())
        || !digits
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ',' | '_' | '.'))
    {
        return None;
    }
    let cleaned: String = if multiplier == 1.0 {
        // Plain counts: `,` `_` and `.` are all thousands separators unless
        // the text is a single decimal like `1200000.0`.
        let dots = digits.matches('.').count();
        let decimal_tail = dots == 1
            && !digits.contains(',')
            && digits
                .rsplit('.')
                .next()
                .is_some_and(|tail| tail.len() != 3);
        if decimal_tail {
            digits.replace('_', "")
        } else {
            digits.replace([',', '_', '.'], "")
        }
    } else {
        digits.replace([',', '_'], "")
    };
    let number: f64 = cleaned.parse().ok()?;
    let rows = (number * multiplier).round();
    (rows.is_finite() && rows >= 0.0).then_some(rows)
}

/// Splits the token stream into statements: on `;` at depth 0, on a T-SQL
/// `GO`, and before a `CREATE` / `ALTER TABLE` that starts at depth 0 (so
/// scripts without terminators still split).
fn split_statements<'s, 'a>(tokens: &'s [Token<'a>], dialect: PlanDialect) -> Vec<&'s [Token<'a>]> {
    let mut statements = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut index = 0usize;
    while index < tokens.len() {
        let token = &tokens[index];
        match token.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth = (depth - 1).max(0),
            // A `;` always ends the statement, even inside an unclosed
            // parenthesis: DDL never nests one, and a cut-off CREATE TABLE
            // must not swallow the rest of the script.
            TokenKind::Semicolon => {
                if index > start {
                    statements.push(&tokens[start..index]);
                }
                start = index + 1;
                depth = 0;
            }
            _ if depth > 0 && index > start && starts_ddl_statement(tokens, index) => {
                statements.push(&tokens[start..index]);
                start = index;
                depth = 0;
            }
            _ if depth == 0 && index > start => {
                let go = dialect == PlanDialect::SqlServer && is_word(token, "GO");
                let create = is_word(token, "CREATE");
                let alter_table = is_word(token, "ALTER")
                    && tokens.get(index + 1).is_some_and(|t| is_word(t, "TABLE"));
                if go {
                    statements.push(&tokens[start..index]);
                    start = index + 1;
                } else if create || alter_table {
                    statements.push(&tokens[start..index]);
                    start = index;
                }
            }
            _ if depth == 0 && dialect == PlanDialect::SqlServer && is_word(token, "GO") => {
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    if start < tokens.len() {
        statements.push(&tokens[start..]);
    }
    statements
}

/// `CREATE TABLE|INDEX|…` or `ALTER TABLE` at `index`: a new statement even
/// when the previous one never closed its parenthesis.
fn starts_ddl_statement(tokens: &[Token<'_>], index: usize) -> bool {
    let next_is = |words: &[&str]| {
        tokens
            .get(index + 1)
            .is_some_and(|token| is_any_word(token, words))
    };
    (is_word(&tokens[index], "CREATE")
        && next_is(&[
            "TABLE",
            "INDEX",
            "UNIQUE",
            "OR",
            "TEMP",
            "TEMPORARY",
            "CLUSTERED",
            "NONCLUSTERED",
            "VIEW",
        ]))
        || (is_word(&tokens[index], "ALTER") && next_is(&["TABLE"]))
}

struct Cursor<'s, 'a> {
    tokens: &'s [Token<'a>],
    pos: usize,
}

impl<'s, 'a> Cursor<'s, 'a> {
    fn new(tokens: &'s [Token<'a>]) -> Self {
        Self { tokens, pos: 0 }
    }

    fn done(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn peek(&self) -> Option<&'s Token<'a>> {
        self.tokens.get(self.pos)
    }

    fn peek_at(&self, offset: usize) -> Option<&'s Token<'a>> {
        self.tokens.get(self.pos + offset)
    }

    fn bump(&mut self) {
        self.pos += 1;
    }

    fn at_word(&self, word: &str) -> bool {
        self.peek().is_some_and(|token| is_word(token, word))
    }

    fn at_any_word(&self, words: &[&str]) -> bool {
        self.peek().is_some_and(|token| is_any_word(token, words))
    }

    fn at_lparen(&self) -> bool {
        self.peek()
            .is_some_and(|token| token.kind == TokenKind::LParen)
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if self.at_word(word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consumes the words in order if they all match.
    fn eat_words(&mut self, words: &[&str]) -> bool {
        let matches = words.iter().enumerate().all(|(offset, word)| {
            self.peek_at(offset)
                .is_some_and(|token| is_word(token, word))
        });
        if matches {
            self.pos += words.len();
        }
        matches
    }

    /// At `(`: returns the tokens inside the matching parentheses and moves
    /// past the `)`. A missing `)` yields everything to the end.
    fn paren_group(&mut self) -> Option<&'s [Token<'a>]> {
        if !self.at_lparen() {
            return None;
        }
        let start = self.pos + 1;
        let mut depth = 0i32;
        let mut index = self.pos;
        while index < self.tokens.len() {
            match self.tokens[index].kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        self.pos = index + 1;
                        return Some(&self.tokens[start..index]);
                    }
                }
                _ => {}
            }
            index += 1;
        }
        self.pos = self.tokens.len();
        Some(&self.tokens[start.min(self.tokens.len())..])
    }

    /// A possibly qualified name: `a`, `a.b`, `[a].[b].[c]`, `a..b`.
    fn name(&mut self, dialect: PlanDialect) -> Option<Vec<String>> {
        let mut parts = Vec::new();
        loop {
            let token = self.peek()?;
            if !token.is_word() {
                return None;
            }
            parts.push(fold_name(
                dialect,
                &token.ident_text(),
                token.ident_quoted(),
            ));
            self.pos += 1;
            let mut dots = 0;
            while self.peek().is_some_and(|t| t.kind == TokenKind::Dot) {
                self.pos += 1;
                dots += 1;
            }
            if dots == 0 {
                break;
            }
            if !self.peek().is_some_and(|t| t.is_word()) {
                break;
            }
        }
        Some(parts)
    }
}

fn schema_and_table(mut parts: Vec<String>) -> (String, String) {
    let table = parts.pop().unwrap_or_default();
    let schema = parts.pop().unwrap_or_default();
    (schema, table)
}

fn split_commas<'s, 'a>(tokens: &'s [Token<'a>]) -> Vec<&'s [Token<'a>]> {
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth -= 1,
            TokenKind::Comma if depth == 0 => {
                items.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < tokens.len() {
        items.push(&tokens[start..]);
    }
    items.retain(|item| !item.is_empty());
    items
}

fn source_text<'x>(src: &'x str, tokens: &[Token<'_>]) -> &'x str {
    match (tokens.first(), tokens.last()) {
        (Some(first), Some(last)) => src
            .get(first.span.start..last.span.end)
            .unwrap_or_default()
            .trim(),
        _ => "",
    }
}

fn normalize_space(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `REFERENCES name [(cols)]`, with the cursor at `REFERENCES`.
fn parse_references(
    cursor: &mut Cursor<'_, '_>,
    dialect: PlanDialect,
) -> Option<(String, String, Vec<String>)> {
    if !cursor.eat_word("REFERENCES") {
        return None;
    }
    let (schema, table) = schema_and_table(cursor.name(dialect)?);
    let columns = cursor
        .paren_group()
        .map(|group| plain_column_names(group, dialect))
        .unwrap_or_default();
    Some((schema, table, columns))
}

fn plain_column_names(tokens: &[Token<'_>], dialect: PlanDialect) -> Vec<String> {
    split_commas(tokens)
        .into_iter()
        .filter_map(|item| {
            let first = item.first()?;
            first
                .is_word()
                .then(|| fold_name(dialect, &first.ident_text(), first.ident_quoted()))
        })
        .collect()
}

struct DdlReader<'x, 'c> {
    src: &'x str,
    dialect: PlanDialect,
    context: &'c mut SchemaContext,
}

impl DdlReader<'_, '_> {
    fn note(&mut self, tokens: &[Token<'_>], reason: SchemaNoteReason) {
        let Some(first) = tokens.first() else {
            return;
        };
        let line = self.src[..first.span.start.min(self.src.len())]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
            + 1;
        let mut snippet = normalize_space(source_text(self.src, tokens));
        if snippet.chars().count() > 80 {
            snippet = snippet.chars().take(77).collect::<String>() + "...";
        }
        self.context.notes.push(SchemaNote {
            line,
            snippet,
            reason,
        });
    }

    fn statement(&mut self, tokens: &[Token<'_>]) {
        let mut cursor = Cursor::new(tokens);
        if cursor.eat_word("CREATE") {
            if cursor.eat_word("OR") {
                // OR REPLACE / OR ALTER
                cursor.bump();
            }
            let mut unique = false;
            let mut clustered = None;
            let mut method: Option<String> = None;
            let mut unsupported = false;
            loop {
                if cursor.eat_word("UNIQUE") {
                    unique = true;
                } else if cursor.eat_word("CLUSTERED") {
                    clustered = Some(true);
                } else if cursor.eat_word("NONCLUSTERED") {
                    clustered = Some(false);
                } else if cursor.eat_word("COLUMNSTORE") {
                    method = Some("columnstore".to_string());
                } else if cursor.eat_word("BITMAP") {
                    method = Some("bitmap".to_string());
                } else if cursor.at_any_word(&["FULLTEXT", "SPATIAL", "XML", "VECTOR"]) {
                    unsupported = true;
                    cursor.bump();
                } else if cursor.at_any_word(&[
                    "TEMP",
                    "TEMPORARY",
                    "UNLOGGED",
                    "GLOBAL",
                    "LOCAL",
                    "PRIVATE",
                    "EXTERNAL",
                    "FOREIGN",
                    "TRANSIENT",
                    "VIRTUAL",
                    "PRIMARY",
                    "XMLTYPE",
                    "ONLINE",
                    "OFFLINE",
                    "SHARDED",
                    "DUPLICATED",
                    "IMMUTABLE",
                    "BLOCKCHAIN",
                ]) {
                    cursor.bump();
                } else {
                    break;
                }
            }
            if cursor.eat_word("TABLE") {
                self.create_table(tokens, &mut cursor);
            } else if cursor.eat_word("INDEX") {
                if unsupported {
                    self.note(tokens, SchemaNoteReason::UnsupportedIndexKind);
                } else {
                    self.create_index(tokens, &mut cursor, unique, clustered, method);
                }
            } else {
                self.context.ignored_statements += 1;
            }
        } else if cursor.eat_words(&["ALTER", "TABLE"]) {
            self.alter_table(tokens, &mut cursor);
        } else {
            self.context.ignored_statements += 1;
        }
    }

    fn eat_if_not_exists(cursor: &mut Cursor<'_, '_>) {
        if !cursor.eat_words(&["IF", "NOT", "EXISTS"]) {
            cursor.eat_words(&["IF", "EXISTS"]);
        }
    }

    fn create_table(&mut self, statement: &[Token<'_>], cursor: &mut Cursor<'_, '_>) {
        Self::eat_if_not_exists(cursor);
        let Some(parts) = cursor.name(self.dialect) else {
            self.note(statement, SchemaNoteReason::TableWithoutColumns);
            return;
        };
        let (schema, name) = schema_and_table(parts);
        // ClickHouse: `ON CLUSTER name`, `UUID '…'` before the column list.
        if cursor.eat_words(&["ON", "CLUSTER"]) {
            cursor.bump();
        }
        if cursor.eat_word("UUID") {
            cursor.bump();
        }
        let Some(body) = cursor.paren_group() else {
            // Keep the table known by name; its columns are not.
            self.context.table_mut(&schema, &name);
            self.note(statement, SchemaNoteReason::TableWithoutColumns);
            return;
        };
        let mut table = SchemaTable {
            schema: schema.clone(),
            name: name.clone(),
            ..Default::default()
        };
        for element in split_commas(body) {
            if !self.table_element(&mut table, element) {
                self.note(element, SchemaNoteReason::MalformedElement);
            }
        }
        self.table_options(&mut table, cursor);
        if table.columns.is_empty() && table.primary_key.is_none() && table.indexes.is_empty() {
            self.context.table_mut(&schema, &name);
            self.note(statement, SchemaNoteReason::TableWithoutColumns);
            return;
        }
        finish_table(&mut table, self.dialect);
        // A stub (index or row count seen first) is replaced, keeping what it
        // had.
        let existing = self.context.table_mut(&schema, &name);
        table.row_count = existing.row_count;
        if existing.columns.is_empty() {
            table.indexes.append(&mut existing.indexes);
            table.foreign_keys.append(&mut existing.foreign_keys);
            if table.primary_key.is_none() {
                table.primary_key = existing.primary_key.take();
            }
        }
        if table.schema.is_empty() {
            table.schema = existing.schema.clone();
        }
        *existing = table;
    }

    /// Trailing table options: ClickHouse `ORDER BY` / `PRIMARY KEY`,
    /// `PARTITION BY`.
    fn table_options(&mut self, table: &mut SchemaTable, cursor: &mut Cursor<'_, '_>) {
        let mut order_by: Option<Vec<SchemaIndexColumn>> = None;
        let mut primary: Option<Vec<SchemaIndexColumn>> = None;
        while !cursor.done() {
            if cursor.eat_words(&["PARTITION", "BY"]) {
                let start = cursor.pos;
                // RANGE (col) / LIST (col) / HASH (col) / expression.
                while !cursor.done()
                    && !cursor.at_lparen()
                    && !cursor.at_any_word(&["ORDER", "PRIMARY", "SETTINGS", "TTL", "SAMPLE"])
                {
                    cursor.bump();
                    if cursor.pos - start >= 2 {
                        break;
                    }
                }
                if cursor.at_lparen() {
                    cursor.paren_group();
                }
                let text =
                    normalize_space(source_text(self.src, &cursor.tokens[start..cursor.pos]));
                if !text.is_empty() {
                    table.partition_key = Some(text);
                }
            } else if self.dialect == PlanDialect::ClickHouse && cursor.eat_words(&["ORDER", "BY"])
            {
                order_by = Some(self.key_expression_list(cursor));
            } else if self.dialect == PlanDialect::ClickHouse
                && cursor.eat_words(&["PRIMARY", "KEY"])
            {
                primary = Some(self.key_expression_list(cursor));
            } else {
                cursor.bump();
            }
        }
        // ClickHouse: the primary index is the PRIMARY KEY when given, else
        // the sorting key. It is sparse and not unique.
        if let Some(columns) = primary.or(order_by)
            && !columns.is_empty()
            && table.primary_key.is_none()
        {
            table.primary_key = Some(SchemaIndex {
                columns,
                primary: true,
                clustered: Some(true),
                ..Default::default()
            });
        }
    }

    /// `(a, b)` or a single bare expression up to the next clause keyword.
    fn key_expression_list(&self, cursor: &mut Cursor<'_, '_>) -> Vec<SchemaIndexColumn> {
        if cursor.at_lparen() {
            let group = cursor.paren_group().unwrap_or_default();
            return self.index_columns(group);
        }
        let start = cursor.pos;
        while !cursor.done()
            && !cursor.at_any_word(&[
                "PARTITION",
                "PRIMARY",
                "ORDER",
                "SAMPLE",
                "TTL",
                "SETTINGS",
                "COMMENT",
                "AS",
            ])
        {
            if cursor.at_lparen() {
                cursor.paren_group();
            } else {
                cursor.bump();
            }
        }
        let tokens = &cursor.tokens[start..cursor.pos];
        if tokens.len() == 1 && is_word(&tokens[0], "tuple") {
            return Vec::new();
        }
        self.index_columns(tokens)
    }

    /// One comma-separated element of a `CREATE TABLE` body. Returns false
    /// when it could not be read.
    fn table_element(&mut self, table: &mut SchemaTable, element: &[Token<'_>]) -> bool {
        let mut cursor = Cursor::new(element);
        let mut constraint_name = String::new();
        if cursor.eat_word("CONSTRAINT") {
            if !cursor.at_any_word(&["PRIMARY", "UNIQUE", "FOREIGN", "CHECK"])
                && let Some(token) = cursor.peek()
                && token.is_word()
            {
                constraint_name = token.ident_text().into_owned();
                cursor.bump();
            }
            return self.table_constraint(table, &mut cursor, constraint_name, element);
        }
        if self.starts_table_constraint(&cursor) {
            return self.table_constraint(table, &mut cursor, constraint_name, element);
        }
        self.column(table, &mut cursor)
    }

    fn starts_table_constraint(&self, cursor: &Cursor<'_, '_>) -> bool {
        let Some(first) = cursor.peek() else {
            return false;
        };
        if first.ident_quoted() {
            return false;
        }
        let next = cursor.peek_at(1);
        let next_is = |word: &str| next.is_some_and(|token| is_word(token, word));
        let next_lparen = next.is_some_and(|token| token.kind == TokenKind::LParen);
        if is_word(first, "PRIMARY") && next_is("KEY") {
            return true;
        }
        if is_word(first, "FOREIGN") && next_is("KEY") {
            return true;
        }
        if is_word(first, "UNIQUE") {
            return true;
        }
        if is_any_word(first, &["CHECK", "EXCLUDE"]) && next_lparen {
            return true;
        }
        if is_any_word(first, &["FULLTEXT", "SPATIAL"])
            && (next_is("KEY") || next_is("INDEX") || next_lparen)
        {
            return true;
        }
        if is_word(first, "PERIOD") && next_is("FOR") {
            return true;
        }
        if is_word(first, "LIKE") {
            return true;
        }
        if is_word(first, "PROJECTION") && self.dialect == PlanDialect::ClickHouse {
            return true;
        }
        let index_word = match self.dialect {
            PlanDialect::MySql | PlanDialect::MariaDb => is_any_word(first, &["KEY", "INDEX"]),
            PlanDialect::SqlServer | PlanDialect::ClickHouse => is_word(first, "INDEX"),
            _ => false,
        };
        if index_word {
            // `KEY (a)` / `KEY name (a)` / `INDEX name NONCLUSTERED (a)` are
            // indexes; `key varchar(20)` is a column named "key".
            if next_lparen {
                return true;
            }
            if self.dialect == PlanDialect::ClickHouse {
                return true;
            }
            let after = cursor.peek_at(2);
            return match after {
                Some(token) if token.kind == TokenKind::LParen => !cursor
                    .peek_at(3)
                    .is_some_and(|inner| matches!(inner.kind, TokenKind::Number(_))),
                Some(token) => {
                    is_any_word(token, &["USING", "UNIQUE", "CLUSTERED", "NONCLUSTERED"])
                }
                None => false,
            };
        }
        false
    }

    fn table_constraint(
        &mut self,
        table: &mut SchemaTable,
        cursor: &mut Cursor<'_, '_>,
        constraint_name: String,
        element: &[Token<'_>],
    ) -> bool {
        if cursor.eat_words(&["PRIMARY", "KEY"]) {
            let clustered = Self::eat_clustered(cursor);
            // MySQL allows `PRIMARY KEY USING BTREE (…)`.
            if cursor.eat_word("USING") {
                cursor.bump();
            }
            let Some(group) = cursor.paren_group() else {
                return false;
            };
            let columns = self.index_columns(group);
            if columns.is_empty() {
                return false;
            }
            table.primary_key = Some(SchemaIndex {
                name: constraint_name,
                columns,
                unique: true,
                primary: true,
                clustered,
                ..Default::default()
            });
            return true;
        }
        if cursor.eat_word("UNIQUE") {
            if !cursor.eat_word("KEY") {
                cursor.eat_word("INDEX");
            }
            let clustered = Self::eat_clustered(cursor);
            let mut name = constraint_name;
            if !cursor.at_lparen()
                && let Some(token) = cursor.peek()
                && token.is_word()
                && !is_word(token, "USING")
            {
                name = token.ident_text().into_owned();
                cursor.bump();
            }
            if cursor.eat_word("USING") {
                cursor.bump();
            }
            let Some(group) = cursor.paren_group() else {
                return false;
            };
            let columns = self.index_columns(group);
            if columns.is_empty() {
                return false;
            }
            table.indexes.push(SchemaIndex {
                name,
                columns,
                unique: true,
                clustered,
                ..Default::default()
            });
            return true;
        }
        if cursor.eat_words(&["FOREIGN", "KEY"]) {
            let mut name = constraint_name;
            if !cursor.at_lparen()
                && let Some(token) = cursor.peek()
                && token.is_word()
            {
                if name.is_empty() {
                    name = token.ident_text().into_owned();
                }
                cursor.bump();
            }
            let Some(group) = cursor.paren_group() else {
                return false;
            };
            let columns = plain_column_names(group, self.dialect);
            let Some((referenced_schema, referenced_table, referenced_columns)) =
                parse_references(cursor, self.dialect)
            else {
                return false;
            };
            if columns.is_empty() {
                return false;
            }
            table.foreign_keys.push(SchemaForeignKey {
                name,
                columns,
                referenced_schema,
                referenced_table,
                referenced_columns,
            });
            return true;
        }
        if cursor.at_any_word(&["FULLTEXT", "SPATIAL"]) {
            self.note(element, SchemaNoteReason::UnsupportedIndexKind);
            return true;
        }
        if cursor.at_any_word(&["KEY", "INDEX"]) {
            cursor.bump();
            return self.inline_index(table, cursor, element);
        }
        // CHECK, EXCLUDE, PERIOD FOR, LIKE, PROJECTION: nothing the findings
        // use.
        true
    }

    fn eat_clustered(cursor: &mut Cursor<'_, '_>) -> Option<bool> {
        if cursor.eat_word("CLUSTERED") {
            Some(true)
        } else if cursor.eat_word("NONCLUSTERED") {
            Some(false)
        } else {
            None
        }
    }

    /// MySQL `KEY name (cols)` / SQL Server `INDEX name [UNIQUE]
    /// [NONCLUSTERED] (cols) [INCLUDE (…)] [WHERE …]`, cursor after the
    /// keyword.
    fn inline_index(
        &mut self,
        table: &mut SchemaTable,
        cursor: &mut Cursor<'_, '_>,
        element: &[Token<'_>],
    ) -> bool {
        let mut index = SchemaIndex::default();
        if !cursor.at_lparen()
            && let Some(token) = cursor.peek()
            && token.is_word()
            && !is_any_word(token, &["USING", "UNIQUE", "CLUSTERED", "NONCLUSTERED"])
        {
            index.name = token.ident_text().into_owned();
            cursor.bump();
        }
        loop {
            if cursor.eat_word("UNIQUE") {
                index.unique = true;
            } else if let Some(clustered) = Self::eat_clustered(cursor) {
                index.clustered = Some(clustered);
            } else if cursor.eat_word("COLUMNSTORE") {
                index.method = Some("columnstore".to_string());
            } else if cursor.eat_word("USING") {
                if let Some(token) = cursor.peek() {
                    index.method = Some(token.text.to_ascii_lowercase());
                }
                cursor.bump();
            } else {
                break;
            }
        }
        if !cursor.at_lparen() {
            // ClickHouse data-skipping index: `INDEX name expr TYPE minmax …`.
            self.note(element, SchemaNoteReason::UnsupportedIndexKind);
            return true;
        }
        let group = cursor.paren_group().unwrap_or_default();
        index.columns = self.index_columns(group);
        if index.columns.is_empty() {
            return false;
        }
        if self.dialect == PlanDialect::ClickHouse {
            self.note(element, SchemaNoteReason::UnsupportedIndexKind);
            return true;
        }
        self.index_tail(&mut index, cursor);
        table.indexes.push(index);
        true
    }

    /// `[USING m] [INCLUDE (…)] [WHERE …]` after an index key list.
    fn index_tail(&self, index: &mut SchemaIndex, cursor: &mut Cursor<'_, '_>) {
        while !cursor.done() {
            if cursor.eat_word("INCLUDE") {
                if let Some(group) = cursor.paren_group() {
                    index.include_columns = plain_column_names(group, self.dialect);
                }
            } else if cursor.eat_word("USING") {
                if let Some(token) = cursor.peek() {
                    index.method = Some(token.text.to_ascii_lowercase());
                }
                cursor.bump();
            } else if cursor.eat_word("WHERE") {
                let start = cursor.pos;
                while !cursor.done()
                    && !cursor.at_any_word(&["WITH", "ON", "TABLESPACE", "FILESTREAM_ON"])
                {
                    if cursor.at_lparen() {
                        cursor.paren_group();
                    } else {
                        cursor.bump();
                    }
                }
                let text =
                    normalize_space(source_text(self.src, &cursor.tokens[start..cursor.pos]));
                if !text.is_empty() {
                    index.filter = Some(text);
                }
            } else if cursor.at_lparen() {
                cursor.paren_group();
            } else {
                cursor.bump();
            }
        }
    }

    /// Key columns of an index or key constraint.
    fn index_columns(&self, tokens: &[Token<'_>]) -> Vec<SchemaIndexColumn> {
        let mut columns = Vec::new();
        for item in split_commas(tokens) {
            let mut end = item.len();
            let mut descending = false;
            // Trailing NULLS FIRST/LAST, then ASC/DESC.
            if end >= 2
                && is_word(&item[end - 2], "NULLS")
                && is_any_word(&item[end - 1], &["FIRST", "LAST"])
            {
                end -= 2;
            }
            if end >= 1 && is_word(&item[end - 1], "DESC") {
                descending = true;
                end -= 1;
            } else if end >= 1 && is_word(&item[end - 1], "ASC") {
                end -= 1;
            }
            let core = &item[..end];
            let Some(first) = core.first() else {
                continue;
            };
            let simple_tail = core[1..].iter().all(|token| token.is_word());
            if first.is_word() && simple_tail {
                // `col`, `col opclass`, `col COLLATE x`.
                columns.push(SchemaIndexColumn {
                    name: fold_name(self.dialect, &first.ident_text(), first.ident_quoted()),
                    descending,
                    expression: false,
                    prefix_length: None,
                });
                continue;
            }
            let mysql = matches!(self.dialect, PlanDialect::MySql | PlanDialect::MariaDb);
            if mysql
                && first.is_word()
                && core.len() == 4
                && core[1].kind == TokenKind::LParen
                && matches!(core[2].kind, TokenKind::Number(_))
                && core[3].kind == TokenKind::RParen
            {
                columns.push(SchemaIndexColumn {
                    name: first.ident_text().into_owned(),
                    descending,
                    expression: false,
                    prefix_length: core[2].text.parse().ok(),
                });
                continue;
            }
            columns.push(SchemaIndexColumn {
                name: normalize_space(source_text(self.src, core)),
                descending,
                expression: true,
                prefix_length: None,
            });
        }
        columns
    }

    fn column(&mut self, table: &mut SchemaTable, cursor: &mut Cursor<'_, '_>) -> bool {
        let Some(name_token) = cursor.peek() else {
            return false;
        };
        if !name_token.is_word() {
            return false;
        }
        let name = fold_name(
            self.dialect,
            &name_token.ident_text(),
            name_token.ident_quoted(),
        );
        cursor.bump();

        // Type: everything up to the first column-constraint keyword.
        let type_start = cursor.pos;
        while let Some(token) = cursor.peek() {
            if token.kind == TokenKind::LParen {
                cursor.paren_group();
                continue;
            }
            if !token.ident_quoted() && token.is_word() {
                let next_is = |word: &str| cursor.peek_at(1).is_some_and(|t| is_word(t, word));
                if is_word(token, "CHARACTER") && next_is("SET") {
                    break;
                }
                if is_any_word(token, &["WITH", "WITHOUT"]) {
                    if next_is("TIME") || next_is("LOCAL") {
                        // timestamp with[out] [local] time zone
                        cursor.bump();
                        continue;
                    }
                    break;
                }
                if is_any_word(token, COLUMN_TYPE_STOP_WORDS) {
                    break;
                }
            }
            if matches!(token.kind, TokenKind::Operator) && token.text != "[" && token.text != "]" {
                // `= default` (ClickHouse shorthand) and similar.
                break;
            }
            cursor.bump();
        }
        let type_tokens = &cursor.tokens[type_start..cursor.pos];
        let type_name = render_type(type_tokens);
        let lower_type = type_name.to_ascii_lowercase();

        let mut column = SchemaColumn {
            name: name.clone(),
            nullable: match self.dialect {
                // ClickHouse columns are NOT NULL unless wrapped in Nullable.
                PlanDialect::ClickHouse => lower_type.contains("nullable("),
                _ => true,
            },
            identity: matches!(
                lower_type.as_str(),
                "serial" | "bigserial" | "smallserial" | "serial4" | "serial8" | "serial2"
            ),
            type_name,
            ..Default::default()
        };
        if column.identity {
            column.nullable = false;
        }

        let mut primary = false;
        let mut primary_descending = false;
        let mut unique = false;
        let mut pending_name = String::new();
        while let Some(token) = cursor.peek() {
            if token.kind == TokenKind::LParen {
                cursor.paren_group();
                continue;
            }
            if cursor.eat_words(&["NOT", "NULL"]) {
                column.nullable = false;
            } else if cursor.eat_word("NULL") {
                column.nullable = true;
            } else if cursor.eat_word("CONSTRAINT") {
                if let Some(token) = cursor.peek()
                    && token.is_word()
                {
                    pending_name = token.ident_text().into_owned();
                    cursor.bump();
                }
            } else if cursor.eat_words(&["PRIMARY", "KEY"]) {
                primary = true;
                column.nullable = false;
                if cursor.eat_word("DESC") {
                    primary_descending = true;
                } else {
                    cursor.eat_word("ASC");
                }
            } else if cursor.eat_word("UNIQUE") {
                cursor.eat_word("KEY");
                unique = true;
            } else if cursor.at_word("REFERENCES") {
                if let Some((referenced_schema, referenced_table, referenced_columns)) =
                    parse_references(cursor, self.dialect)
                {
                    table.foreign_keys.push(SchemaForeignKey {
                        name: std::mem::take(&mut pending_name),
                        columns: vec![name.clone()],
                        referenced_schema,
                        referenced_table,
                        referenced_columns,
                    });
                }
            } else if cursor.eat_word("COLLATE") {
                if let Some(token) = cursor.peek() {
                    column.collation = Some(token.ident_text().into_owned());
                    cursor.bump();
                }
            } else if cursor.at_any_word(&["IDENTITY", "AUTO_INCREMENT", "AUTOINCREMENT"]) {
                column.identity = true;
                cursor.bump();
            } else if cursor.eat_word("GENERATED") {
                // GENERATED { ALWAYS | BY DEFAULT [ON NULL] } AS { IDENTITY | (expr) }
                while cursor.at_any_word(&["ALWAYS", "BY", "DEFAULT", "ON", "NULL"]) {
                    cursor.bump();
                }
                if cursor.eat_word("AS") {
                    if cursor.eat_word("IDENTITY") {
                        column.identity = true;
                        column.nullable = false;
                    } else {
                        column.computed = true;
                    }
                }
            } else if cursor.at_any_word(&["AS", "MATERIALIZED", "ALIAS", "EPHEMERAL"]) {
                column.computed = true;
                cursor.bump();
            } else if cursor.eat_word("DEFAULT") {
                // Skip the default expression's first token so `DEFAULT NULL`
                // is not read as a nullability clause.
                if cursor.at_lparen() {
                    cursor.paren_group();
                } else {
                    cursor.bump();
                }
            } else {
                cursor.bump();
            }
        }

        if primary {
            table.primary_key = Some(SchemaIndex {
                name: std::mem::take(&mut pending_name),
                columns: vec![SchemaIndexColumn {
                    name: name.clone(),
                    descending: primary_descending,
                    ..Default::default()
                }],
                unique: true,
                primary: true,
                ..Default::default()
            });
        }
        if unique {
            table.indexes.push(SchemaIndex {
                name: pending_name,
                columns: vec![SchemaIndexColumn {
                    name: name.clone(),
                    ..Default::default()
                }],
                unique: true,
                ..Default::default()
            });
        }
        table.columns.push(column);
        true
    }

    fn create_index(
        &mut self,
        statement: &[Token<'_>],
        cursor: &mut Cursor<'_, '_>,
        unique: bool,
        clustered: Option<bool>,
        method: Option<String>,
    ) {
        cursor.eat_word("CONCURRENTLY");
        Self::eat_if_not_exists(cursor);
        let mut index = SchemaIndex {
            unique,
            clustered,
            method,
            ..Default::default()
        };
        if !cursor.at_word("ON")
            && let Some(parts) = cursor.name(self.dialect)
        {
            index.name = parts.last().cloned().unwrap_or_default();
        }
        // MySQL: `CREATE INDEX name USING BTREE ON t (…)`.
        if cursor.eat_word("USING") {
            if let Some(token) = cursor.peek() {
                index.method = Some(token.text.to_ascii_lowercase());
            }
            cursor.bump();
        }
        if !cursor.eat_word("ON") {
            self.note(statement, SchemaNoteReason::MalformedIndex);
            return;
        }
        cursor.eat_word("ONLY");
        let Some(parts) = cursor.name(self.dialect) else {
            self.note(statement, SchemaNoteReason::MalformedIndex);
            return;
        };
        let (schema, table_name) = schema_and_table(parts);
        if cursor.eat_word("USING") {
            if let Some(token) = cursor.peek() {
                index.method = Some(token.text.to_ascii_lowercase());
            }
            cursor.bump();
        }
        let Some(group) = cursor.paren_group() else {
            if index.method.as_deref() == Some("columnstore") {
                // CREATE CLUSTERED COLUMNSTORE INDEX has no key list: the
                // index is the whole table. Kept so the table is not taken
                // for a heap; with no key columns it never matches a seek.
                let table = self.context.table_mut(&schema, &table_name);
                let known = !index.name.is_empty()
                    && table
                        .all_indexes()
                        .any(|existing| existing.name.eq_ignore_ascii_case(&index.name));
                if !known {
                    table.indexes.push(index);
                }
            } else {
                self.note(statement, SchemaNoteReason::MalformedIndex);
            }
            return;
        };
        index.columns = self.index_columns(group);
        if index.columns.is_empty() {
            self.note(statement, SchemaNoteReason::MalformedIndex);
            return;
        }
        self.index_tail(&mut index, cursor);
        let table = self.context.table_mut(&schema, &table_name);
        if !index.name.is_empty()
            && table
                .all_indexes()
                .any(|existing| existing.name.eq_ignore_ascii_case(&index.name))
        {
            return;
        }
        table.indexes.push(index);
    }

    fn alter_table(&mut self, statement: &[Token<'_>], cursor: &mut Cursor<'_, '_>) {
        cursor.eat_word("ONLY");
        Self::eat_if_not_exists(cursor);
        cursor.eat_word("ONLY");
        let Some(parts) = cursor.name(self.dialect) else {
            self.note(statement, SchemaNoteReason::MalformedAlter);
            return;
        };
        let (schema, name) = schema_and_table(parts);
        // SQL Server: `WITH CHECK ADD CONSTRAINT …` / `WITH NOCHECK ADD …`.
        if cursor.eat_word("WITH") {
            cursor.bump();
        }
        let rest = &cursor.tokens[cursor.pos..];
        let mut table = std::mem::take(self.context.table_mut(&schema, &name));
        let mut understood = false;
        let mut failed = false;
        for action in split_commas(rest) {
            let mut action_cursor = Cursor::new(action);
            if action_cursor.eat_word("ADD") {
                let before = (
                    table.columns.len(),
                    table.indexes.len(),
                    table.foreign_keys.len(),
                    table.primary_key.is_some(),
                );
                let element = &action[action_cursor.pos..];
                let mut element_cursor = Cursor::new(element);
                let ok = if element_cursor.eat_word("COLUMN") {
                    Self::eat_if_not_exists(&mut element_cursor);
                    self.column(&mut table, &mut element_cursor)
                } else if element.first().is_some_and(|t| t.kind == TokenKind::LParen) {
                    // Oracle: ADD (col type, col type) / ADD (CONSTRAINT …).
                    let group = element_cursor.paren_group().unwrap_or_default();
                    split_commas(group)
                        .into_iter()
                        .all(|inner| self.table_element(&mut table, inner))
                } else {
                    self.table_element(&mut table, element)
                };
                let after = (
                    table.columns.len(),
                    table.indexes.len(),
                    table.foreign_keys.len(),
                    table.primary_key.is_some(),
                );
                if ok && before != after {
                    understood = true;
                } else if !ok {
                    failed = true;
                } else {
                    // ADD CHECK and similar: read, nothing to keep.
                    understood = true;
                }
            } else if action_cursor.eat_word("ALTER") || action_cursor.eat_word("MODIFY") {
                // ALTER [COLUMN] c SET NOT NULL / DROP NOT NULL (PostgreSQL).
                action_cursor.eat_word("COLUMN");
                if let Some(token) = action_cursor.peek()
                    && token.is_word()
                {
                    let column_name =
                        fold_name(self.dialect, &token.ident_text(), token.ident_quoted());
                    action_cursor.bump();
                    let set = action_cursor.eat_words(&["SET", "NOT", "NULL"]);
                    let drop = !set && action_cursor.eat_words(&["DROP", "NOT", "NULL"]);
                    if (set || drop)
                        && let Some(column) = table
                            .columns
                            .iter_mut()
                            .find(|c| c.name.eq_ignore_ascii_case(&column_name))
                    {
                        column.nullable = drop;
                    }
                }
                understood = true;
            } else {
                // OWNER TO, ENABLE TRIGGER, CHECK CONSTRAINT, DROP …: not
                // schema shape the findings use.
                understood = true;
            }
        }
        finish_table(&mut table, self.dialect);
        *self.context.table_mut(&schema, &name) = table;
        if failed || !understood {
            self.note(statement, SchemaNoteReason::MalformedAlter);
        }
    }
}

const COLUMN_TYPE_STOP_WORDS: &[&str] = &[
    "NOT",
    "NULL",
    "DEFAULT",
    "PRIMARY",
    "UNIQUE",
    "REFERENCES",
    "CHECK",
    "CONSTRAINT",
    "COLLATE",
    "GENERATED",
    "IDENTITY",
    "AUTO_INCREMENT",
    "AUTOINCREMENT",
    "COMMENT",
    "AS",
    "CODEC",
    "MATERIALIZED",
    "ALIAS",
    "EPHEMERAL",
    "TTL",
    "SPARSE",
    "ROWGUIDCOL",
    "VISIBLE",
    "INVISIBLE",
    "ON",
    "KEY",
    "ENCRYPTED",
    "MASKED",
    "PERSISTED",
    "STORED",
    "VIRTUAL",
    "INDEX",
    "CHARSET",
    "ENABLE",
    "DISABLE",
    "USING",
    "STORAGE",
    "COMPRESSION",
    "SRID",
    "FILESTREAM",
    "HIDDEN",
];

fn render_type(tokens: &[Token<'_>]) -> String {
    let mut out = String::new();
    for (index, token) in tokens.iter().enumerate() {
        let tight_before = matches!(
            token.kind,
            TokenKind::LParen | TokenKind::RParen | TokenKind::Comma | TokenKind::Dot
        ) || token.text == "["
            || token.text == "]";
        let previous = index.checked_sub(1).map(|i| &tokens[i]);
        let tight_after_previous = previous.is_some_and(|p| {
            matches!(
                p.kind,
                TokenKind::LParen | TokenKind::Dot | TokenKind::Comma
            ) || p.text == "["
        });
        if !out.is_empty() && !tight_before && !tight_after_previous {
            out.push(' ');
        }
        // SQL Server scripts bracket type names: `[datetime2](7)`.
        if matches!(token.kind, TokenKind::Ident(IdentQuote::Bracket)) {
            out.push_str(&token.ident_text());
        } else {
            out.push_str(token.text);
        }
    }
    out
}

/// Primary-key columns are never nullable; SQLite's rowid tables and
/// engines without a declared key keep `primary_key: None`.
fn finish_table(table: &mut SchemaTable, _dialect: PlanDialect) {
    if let Some(primary) = &table.primary_key
        && !primary.columns.iter().any(|c| c.expression)
        && _dialect != PlanDialect::ClickHouse
    {
        let names: Vec<String> = primary.columns.iter().map(|c| c.name.clone()).collect();
        for column in &mut table.columns {
            if names.iter().any(|n| n.eq_ignore_ascii_case(&column.name)) {
                // SQLite allows NULL in a non-INTEGER primary key of a rowid
                // table, but treating key columns as NOT NULL only makes the
                // nullable-NOT-IN rule quieter, never wrong the loud way.
                column.nullable = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(index: &SchemaIndex) -> Vec<&str> {
        index.key_names()
    }

    #[test]
    fn postgres_tables_indexes_and_constraints() {
        let ddl = r#"
            CREATE TABLE public.customers (
                id bigserial PRIMARY KEY,
                email text NOT NULL,
                name character varying(200),
                created_at timestamp with time zone DEFAULT now() NOT NULL,
                balance numeric(12, 2) DEFAULT NULL
            );
            CREATE TABLE IF NOT EXISTS orders (
                id bigint GENERATED ALWAYS AS IDENTITY,
                customer_id integer REFERENCES customers (id),
                status text,
                total numeric(12,2),
                created_at timestamptz NOT NULL,
                CONSTRAINT orders_pkey PRIMARY KEY (id),
                CONSTRAINT orders_total_check CHECK (total >= 0 AND status IS NOT NULL)
            );
            CREATE UNIQUE INDEX customers_email_lower_idx ON public.customers (lower(email));
            CREATE INDEX CONCURRENTLY orders_customer_created_idx
                ON ONLY orders USING btree (customer_id, created_at DESC NULLS LAST)
                INCLUDE (total) WHERE status <> 'cancelled';
            ALTER TABLE ONLY orders ADD CONSTRAINT orders_customer_fk
                FOREIGN KEY (customer_id) REFERENCES public.customers(id) ON DELETE CASCADE;
            orders: 1.2M
            -- customers: 250,000 rows
        "#;
        let schema = SchemaContext::from_ddl(ddl, PlanDialect::Postgres);
        assert!(schema.notes.is_empty(), "{:?}", schema.notes);

        let customers = schema
            .table(Some("public"), "customers")
            .expect("customers");
        assert_eq!(customers.schema, "public");
        assert_eq!(customers.row_count, Some(250_000.0));
        assert_eq!(names(customers.primary_key.as_ref().unwrap()), ["id"]);
        let email = customers.column("email").unwrap();
        assert_eq!(email.type_name, "text");
        assert!(!email.nullable);
        let name = customers.column("name").unwrap();
        assert_eq!(name.type_name, "character varying(200)");
        assert!(name.nullable);
        let created = customers.column("created_at").unwrap();
        assert_eq!(created.type_name, "timestamp with time zone");
        assert!(!created.nullable);
        assert!(customers.column("balance").unwrap().nullable);
        assert!(customers.column("id").unwrap().identity);
        let lower = customers.index("customers_email_lower_idx").unwrap();
        assert!(lower.unique);
        assert!(lower.columns[0].expression);
        assert_eq!(lower.columns[0].name, "lower(email)");

        let orders = schema.table(None, "ORDERS").expect("orders");
        assert_eq!(orders.row_count, Some(1_200_000.0));
        assert_eq!(orders.primary_key.as_ref().unwrap().name, "orders_pkey");
        assert!(!orders.column("id").unwrap().nullable);
        assert!(orders.column("customer_id").unwrap().nullable);
        let index = orders.index("orders_customer_created_idx").unwrap();
        assert_eq!(names(index), ["customer_id", "created_at"]);
        assert!(index.columns[1].descending);
        assert_eq!(index.include_columns, ["total"]);
        assert_eq!(index.filter.as_deref(), Some("status <> 'cancelled'"));
        assert_eq!(index.method.as_deref(), Some("btree"));
        // Column-level REFERENCES plus the ALTER TABLE constraint.
        assert_eq!(orders.foreign_keys.len(), 2);
        assert_eq!(orders.foreign_keys[1].name, "orders_customer_fk");
        assert_eq!(orders.foreign_keys[1].referenced_table, "customers");
        assert_eq!(orders.foreign_keys[1].referenced_columns, ["id"]);
    }

    #[test]
    fn postgres_folds_unquoted_names_and_keeps_quoted_ones() {
        let schema = SchemaContext::from_ddl(
            r#"CREATE TABLE "Order Items" (ID int primary key, "SKU" text); CREATE INDEX IX ON "Order Items" ("SKU");"#,
            PlanDialect::Postgres,
        );
        let table = schema.table(None, "Order Items").unwrap();
        assert_eq!(table.columns[0].name, "id");
        assert_eq!(table.columns[1].name, "SKU");
        assert_eq!(table.indexes[0].name, "ix");
    }

    #[test]
    fn sql_server_brackets_clustered_include_and_filter() {
        let ddl = r#"
            CREATE TABLE [dbo].[Orders](
                [OrderID] [int] IDENTITY(1,1) NOT NULL,
                [CustomerID] [int] NOT NULL,
                [OrderDate] [datetime2](7) NOT NULL,
                [Status] [varchar](20) COLLATE SQL_Latin1_General_CP1_CI_AS NULL,
                [Total] [decimal](12, 2) NULL,
                [Notes] [nvarchar](max) NULL,
             CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED ([OrderID] ASC)
                WITH (PAD_INDEX = OFF, STATISTICS_NORECOMPUTE = OFF) ON [PRIMARY],
             INDEX IX_Orders_Status NONCLUSTERED ([Status])
            ) ON [PRIMARY]
            GO
            CREATE NONCLUSTERED INDEX [IX_Orders_Customer] ON [dbo].[Orders] ([CustomerID] ASC, [OrderDate] DESC)
                INCLUDE ([Total], [Status]) WHERE ([Status] <> 'X') WITH (FILLFACTOR = 90) ON [PRIMARY]
            GO
            ALTER TABLE [dbo].[Orders] WITH CHECK ADD CONSTRAINT [FK_Orders_Customers]
                FOREIGN KEY([CustomerID]) REFERENCES [dbo].[Customers] ([CustomerID])
            GO
            ALTER TABLE [dbo].[Orders] CHECK CONSTRAINT [FK_Orders_Customers]
            GO
            CREATE TABLE dbo.Heap (a int, b varchar(10))
            CREATE UNIQUE CLUSTERED INDEX CX_Heap ON dbo.Heap (a)
        "#;
        let schema = SchemaContext::from_ddl(ddl, PlanDialect::SqlServer);
        assert!(schema.notes.is_empty(), "{:?}", schema.notes);
        let orders = schema.table(Some("dbo"), "Orders").unwrap();
        let pk = orders.primary_key.as_ref().unwrap();
        assert_eq!(pk.name, "PK_Orders");
        assert_eq!(pk.clustered, Some(true));
        assert_eq!(names(pk), ["OrderID"]);
        assert_eq!(
            orders.column("OrderDate").unwrap().type_name,
            "datetime2(7)"
        );
        assert_eq!(
            orders.column("OrderDate").unwrap().type_family(),
            TypeFamily::Temporal
        );
        let status = orders.column("status").unwrap();
        assert!(status.nullable);
        assert_eq!(
            status.collation.as_deref(),
            Some("SQL_Latin1_General_CP1_CI_AS")
        );
        assert!(orders.column("OrderID").unwrap().identity);
        assert!(!orders.column("OrderID").unwrap().nullable);
        assert_eq!(
            orders.index("IX_Orders_Status").unwrap().clustered,
            Some(false)
        );
        let index = orders.index("ix_orders_customer").unwrap();
        assert_eq!(names(index), ["CustomerID", "OrderDate"]);
        assert!(index.columns[1].descending);
        assert_eq!(index.include_columns, ["Total", "Status"]);
        assert_eq!(index.filter.as_deref(), Some("([Status] <> 'X')"));
        assert_eq!(index.clustered, Some(false));
        assert_eq!(orders.foreign_keys.len(), 1);
        assert_eq!(orders.foreign_keys[0].referenced_table, "Customers");
        assert_eq!(orders.foreign_keys[0].referenced_schema, "dbo");
        // Customers is only known by name through nothing: the FK does not
        // create a stub.
        assert!(schema.table(None, "Customers").is_none());
        let heap = schema.table(None, "Heap").unwrap();
        assert!(heap.primary_key.is_none());
        assert_eq!(heap.indexes[0].clustered, Some(true));
        assert!(heap.indexes[0].unique);
    }

    #[test]
    fn mysql_inline_keys_prefixes_and_alter() {
        let ddl = r#"
            CREATE TABLE `orders` (
              `id` bigint unsigned NOT NULL AUTO_INCREMENT,
              `customer_id` int NOT NULL,
              `status` varchar(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci DEFAULT NULL,
              `key` varchar(10) DEFAULT NULL,
              `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
              `notes` text,
              PRIMARY KEY (`id`),
              UNIQUE KEY `uq_orders_key` (`key`),
              KEY `idx_orders_customer` (`customer_id`,`created_at` DESC),
              KEY `idx_orders_notes` (`notes`(50)) USING BTREE,
              FULLTEXT KEY `ft_notes` (`notes`),
              CONSTRAINT `fk_orders_customer` FOREIGN KEY (`customer_id`) REFERENCES `customers` (`id`)
            ) ENGINE=InnoDB AUTO_INCREMENT=1001 DEFAULT CHARSET=utf8mb4;
            ALTER TABLE `orders` ADD INDEX `idx_orders_status` (`status`), ADD COLUMN `total` decimal(12,2) NULL;
            CREATE INDEX idx_orders_created USING BTREE ON orders (created_at);
        "#;
        let schema = SchemaContext::from_ddl(ddl, PlanDialect::MySql);
        let table = schema.table(None, "orders").unwrap();
        assert_eq!(table.columns.len(), 7);
        assert_eq!(table.column("id").unwrap().type_name, "bigint unsigned");
        assert!(table.column("id").unwrap().identity);
        let status = table.column("status").unwrap();
        assert_eq!(status.type_name, "varchar(20)");
        assert_eq!(status.collation.as_deref(), Some("utf8mb4_0900_ai_ci"));
        assert!(status.nullable);
        assert_eq!(table.column("key").unwrap().type_name, "varchar(10)");
        assert!(!table.column("created_at").unwrap().nullable);
        assert_eq!(names(table.primary_key.as_ref().unwrap()), ["id"]);
        assert!(table.index("uq_orders_key").unwrap().unique);
        let customer = table.index("idx_orders_customer").unwrap();
        assert_eq!(names(customer), ["customer_id", "created_at"]);
        assert!(customer.columns[1].descending);
        assert_eq!(
            table.index("idx_orders_notes").unwrap().columns[0].prefix_length,
            Some(50)
        );
        assert!(table.index("idx_orders_status").is_some());
        assert!(table.index("idx_orders_created").is_some());
        assert_eq!(table.column("total").unwrap().type_name, "decimal(12,2)");
        assert_eq!(table.foreign_keys[0].referenced_table, "customers");
        // The FULLTEXT key is reported, not silently dropped.
        assert_eq!(schema.notes.len(), 1);
        assert_eq!(
            schema.notes[0].reason,
            SchemaNoteReason::UnsupportedIndexKind
        );
    }

    #[test]
    fn oracle_folds_to_upper_and_reads_out_of_line_constraints() {
        let ddl = r#"
            CREATE TABLE shop.orders (
                order_id     NUMBER(10)        NOT NULL,
                customer_id  NUMBER(10)        NOT NULL,
                status       VARCHAR2(20 CHAR),
                created_at   TIMESTAMP(6) WITH LOCAL TIME ZONE DEFAULT SYSTIMESTAMP NOT NULL ENABLE,
                CONSTRAINT orders_pk PRIMARY KEY (order_id) USING INDEX TABLESPACE users
            ) TABLESPACE users;
            CREATE INDEX shop.orders_cust_ix ON shop.orders (customer_id, UPPER(status)) TABLESPACE users;
            ALTER TABLE shop.orders ADD (
                CONSTRAINT orders_cust_fk FOREIGN KEY (customer_id) REFERENCES shop.customers (customer_id)
            );
        "#;
        let schema = SchemaContext::from_ddl(ddl, PlanDialect::Oracle);
        assert!(schema.notes.is_empty(), "{:?}", schema.notes);
        let table = schema.table(Some("SHOP"), "ORDERS").unwrap();
        assert_eq!(table.name, "ORDERS");
        assert_eq!(
            table.column("STATUS").unwrap().type_name,
            "VARCHAR2(20 CHAR)"
        );
        assert_eq!(
            table.column("status").unwrap().type_family(),
            TypeFamily::Text { unicode: false }
        );
        assert_eq!(
            table.column("created_at").unwrap().type_name,
            "TIMESTAMP(6) WITH LOCAL TIME ZONE"
        );
        assert!(!table.column("created_at").unwrap().nullable);
        assert_eq!(names(table.primary_key.as_ref().unwrap()), ["ORDER_ID"]);
        let index = table.index("orders_cust_ix").unwrap();
        assert_eq!(index.columns[0].name, "CUSTOMER_ID");
        assert!(index.columns[1].expression);
        assert_eq!(table.foreign_keys[0].referenced_table, "CUSTOMERS");
    }

    #[test]
    fn sqlite_typeless_columns_and_partial_index() {
        let ddl = r#"
            CREATE TABLE customers (id INTEGER PRIMARY KEY AUTOINCREMENT, email TEXT UNIQUE, blob_col, [weird name] TEXT);
            CREATE TABLE orders (
                id INTEGER PRIMARY KEY,
                customer_id INTEGER NOT NULL REFERENCES customers(id),
                status TEXT,
                FOREIGN KEY (customer_id) REFERENCES customers
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS idx_open ON orders(customer_id) WHERE status = 'open';
        "#;
        let schema = SchemaContext::from_ddl(ddl, PlanDialect::Sqlite);
        assert!(schema.notes.is_empty(), "{:?}", schema.notes);
        let customers = schema.table(None, "customers").unwrap();
        assert_eq!(customers.column("blob_col").unwrap().type_name, "");
        assert!(customers.column("weird name").is_some());
        assert!(customers.indexes[0].unique);
        assert_eq!(names(&customers.indexes[0]), ["email"]);
        let orders = schema.table(None, "orders").unwrap();
        assert_eq!(orders.foreign_keys.len(), 2);
        assert!(orders.foreign_keys[1].referenced_columns.is_empty());
        assert_eq!(
            orders.index("idx_open").unwrap().filter.as_deref(),
            Some("status = 'open'")
        );
    }

    #[test]
    fn duckdb_reads_like_postgres() {
        let schema = SchemaContext::from_ddl(
            "CREATE OR REPLACE TABLE main.orders (id BIGINT PRIMARY KEY, total DECIMAL(12,2), tags VARCHAR[], created TIMESTAMP);\nCREATE INDEX orders_created ON orders (created);",
            PlanDialect::DuckDb,
        );
        assert!(schema.notes.is_empty(), "{:?}", schema.notes);
        let table = schema.table(None, "orders").unwrap();
        assert_eq!(table.schema, "main");
        assert_eq!(table.column("tags").unwrap().type_name, "VARCHAR[]");
        assert_eq!(
            table.column("total").unwrap().type_family(),
            TypeFamily::Decimal
        );
        assert_eq!(names(&table.indexes[0]), ["created"]);
    }

    #[test]
    fn clickhouse_sorting_key_is_the_primary_index() {
        let ddl = r#"
            CREATE TABLE shop.events ON CLUSTER main
            (
                `event_date` Date,
                `user_id` UInt64,
                `kind` LowCardinality(String),
                `payload` Nullable(String) CODEC(ZSTD(3)),
                INDEX idx_kind kind TYPE set(100) GRANULARITY 4
            )
            ENGINE = MergeTree
            PARTITION BY toYYYYMM(event_date)
            ORDER BY (user_id, event_date)
            SETTINGS index_granularity = 8192;
        "#;
        let schema = SchemaContext::from_ddl(ddl, PlanDialect::ClickHouse);
        let table = schema.table(Some("shop"), "events").unwrap();
        assert_eq!(table.columns.len(), 4);
        assert!(!table.column("user_id").unwrap().nullable);
        assert!(table.column("payload").unwrap().nullable);
        assert_eq!(
            table.column("kind").unwrap().type_family(),
            TypeFamily::Text { unicode: false }
        );
        let primary = table.primary_key.as_ref().unwrap();
        assert_eq!(names(primary), ["user_id", "event_date"]);
        assert!(!primary.unique);
        assert_eq!(table.partition_key.as_deref(), Some("toYYYYMM(event_date)"));
        assert_eq!(schema.notes.len(), 1);
        assert_eq!(
            schema.notes[0].reason,
            SchemaNoteReason::UnsupportedIndexKind
        );
    }

    #[test]
    fn broken_ddl_keeps_what_it_can_and_reports_the_rest() {
        let ddl = r#"
            SET search_path = shop;
            CREATE TABLE a (id int PRIMARY KEY, name text
            CREATE TABLE b AS SELECT * FROM a;
            CREATE INDEX ON ;
            CREATE INDEX b_name ON b (name);
            ALTER TABLE ;
            CREATE VIEW v AS SELECT 1;
            INSERT INTO a VALUES (1, 'x');
            CREATE TABLE c (id int, , CONSTRAINT c_pk PRIMARY KEY, name text);
        "#;
        let schema = SchemaContext::from_ddl(ddl, PlanDialect::Postgres);
        // `a` was cut off before its closing parenthesis but its columns are
        // still read.
        let a = schema.table(None, "a").unwrap();
        assert_eq!(a.columns.len(), 2);
        assert!(a.primary_key.is_some());
        // `b` is known by name with an index, without columns.
        let b = schema.table(None, "b").unwrap();
        assert!(!b.has_columns());
        assert_eq!(names(&b.indexes[0]), ["name"]);
        let c = schema.table(None, "c").unwrap();
        assert_eq!(c.columns.len(), 2);
        let reasons: Vec<SchemaNoteReason> = schema.notes.iter().map(|n| n.reason).collect();
        assert_eq!(
            reasons,
            [
                SchemaNoteReason::TableWithoutColumns,
                SchemaNoteReason::MalformedIndex,
                SchemaNoteReason::MalformedAlter,
                SchemaNoteReason::MalformedElement,
            ]
        );
        assert_eq!(schema.notes[0].line, 4);
        assert!(
            schema.notes[0]
                .snippet
                .starts_with("CREATE TABLE b AS SELECT")
        );
        assert_eq!(schema.ignored_statements, 3);
        let message = schema.notes[1].message();
        assert_eq!(
            message.key(),
            Some("services.plans.schema.note.malformed_index")
        );
        assert!(
            message
                .arguments()
                .any(|(name, value)| name == "line" && value == "5")
        );
    }

    #[test]
    fn row_count_lines_parse_without_ddl() {
        let schema = SchemaContext::from_ddl(
            "orders: 1200000\npublic.customers: 250k\n\"Line Items\": 3,400,000 rows\nnot a count: maybe\nx: 12.5\n",
            PlanDialect::Postgres,
        );
        assert_eq!(schema.table(None, "orders").unwrap().row_count, Some(1.2e6));
        assert_eq!(
            schema.table(Some("public"), "customers").unwrap().row_count,
            Some(250_000.0)
        );
        assert_eq!(
            schema.table(None, "Line Items").unwrap().row_count,
            Some(3_400_000.0)
        );
        assert_eq!(schema.table(None, "x").unwrap().row_count, Some(13.0));
        assert_eq!(schema.tables.len(), 4);
    }

    #[test]
    fn type_families() {
        assert_eq!(TypeFamily::of("INT"), TypeFamily::Integer);
        assert_eq!(TypeFamily::of("bigint unsigned"), TypeFamily::Integer);
        assert_eq!(TypeFamily::of("numeric(12,2)"), TypeFamily::Decimal);
        assert_eq!(TypeFamily::of("double precision"), TypeFamily::Float);
        assert_eq!(
            TypeFamily::of("nvarchar(200)"),
            TypeFamily::Text { unicode: true }
        );
        assert_eq!(
            TypeFamily::of("character varying(20)"),
            TypeFamily::Text { unicode: false }
        );
        assert_eq!(
            TypeFamily::of("timestamp with time zone"),
            TypeFamily::Temporal
        );
        assert_eq!(TypeFamily::of("Nullable(UInt64)"), TypeFamily::Integer);
        assert_eq!(TypeFamily::of("uniqueidentifier"), TypeFamily::Uuid);
        assert_eq!(TypeFamily::of("geometry"), TypeFamily::Other);
    }

    #[test]
    fn from_catalog_builds_the_same_shape() {
        let columns = vec![
            ColumnInfo {
                name: "Email".into(),
                ordinal: 2,
                type_name: "nvarchar".into(),
                max_length: 400,
                precision: 0,
                scale: 0,
                is_nullable: true,
                is_identity: false,
                is_computed: false,
                default_value: None,
                collation: Some("Latin1_General_CI_AS".into()),
            },
            ColumnInfo {
                name: "CustomerID".into(),
                ordinal: 1,
                type_name: "int".into(),
                max_length: 4,
                precision: 10,
                scale: 0,
                is_nullable: false,
                is_identity: true,
                is_computed: false,
                default_value: None,
                collation: None,
            },
        ];
        let indexes = vec![
            IndexInfo {
                name: "PK_Customers".into(),
                type_desc: "CLUSTERED".into(),
                is_unique: true,
                is_primary_key: true,
                columns: vec!["CustomerID".into()],
                included_columns: vec![],
                filter_definition: None,
            },
            IndexInfo {
                name: "IX_Email".into(),
                type_desc: "NONCLUSTERED".into(),
                is_unique: false,
                is_primary_key: false,
                columns: vec!["Email DESC".into()],
                included_columns: vec!["CustomerID".into()],
                filter_definition: Some("([Email] IS NOT NULL)".into()),
            },
        ];
        let constraints = vec![ConstraintInfo {
            name: "FK_Customers_Regions".into(),
            type_desc: "FOREIGN_KEY_CONSTRAINT".into(),
            columns: vec!["RegionID".into()],
            definition: Some(
                "FOREIGN KEY ([RegionID]) REFERENCES [dbo].[Regions] ([RegionID])".into(),
            ),
        }];
        let table = SchemaTable::from_catalog(
            PlanDialect::SqlServer,
            "dbo",
            "Customers",
            &columns,
            &indexes,
            &constraints,
            Some(250_000.0),
        );
        assert_eq!(table.columns[0].name, "CustomerID");
        assert_eq!(table.columns[1].type_name, "nvarchar(200)");
        assert_eq!(table.primary_key.as_ref().unwrap().clustered, Some(true));
        assert!(table.indexes[0].columns[0].descending);
        assert_eq!(table.indexes[0].columns[0].name, "Email");
        assert_eq!(
            table.indexes[0].filter.as_deref(),
            Some("([Email] IS NOT NULL)")
        );
        assert_eq!(table.foreign_keys[0].referenced_table, "Regions");
        assert_eq!(table.foreign_keys[0].referenced_columns, ["RegionID"]);
        let schema = SchemaContext::from_tables(PlanDialect::SqlServer, vec![table]);
        assert!(schema.table(Some("dbo"), "[Customers]").is_some());
    }
}
