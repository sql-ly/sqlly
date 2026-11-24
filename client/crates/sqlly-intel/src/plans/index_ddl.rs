//! Shared `CREATE INDEX` generator for findings, the index designer, and
//! copy-to-editor actions.

use crate::plans::PlanDialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexDdlDialect {
    SqlServer,
    Postgres,
    MySql,
    MariaDb,
    Oracle,
    Sqlite,
    DuckDb,
}

impl IndexDdlDialect {
    /// The DDL dialect for a plan dialect. ClickHouse has no secondary
    /// B-tree indexes to create, so it has no entry.
    pub fn for_plan(dialect: PlanDialect) -> Option<Self> {
        match dialect {
            PlanDialect::SqlServer => Some(Self::SqlServer),
            PlanDialect::Postgres => Some(Self::Postgres),
            PlanDialect::MySql => Some(Self::MySql),
            PlanDialect::MariaDb => Some(Self::MariaDb),
            PlanDialect::Oracle => Some(Self::Oracle),
            PlanDialect::Sqlite => Some(Self::Sqlite),
            PlanDialect::DuckDb => Some(Self::DuckDb),
            PlanDialect::ClickHouse => None,
        }
    }

    /// True when the engine stores non-key columns in an index leaf
    /// (`INCLUDE`). Everywhere else covering columns have to be key columns.
    pub fn supports_include(self) -> bool {
        matches!(self, Self::SqlServer | Self::Postgres)
    }

    /// True when the engine accepts a `WHERE` clause on an index.
    pub fn supports_filter(self) -> bool {
        matches!(self, Self::SqlServer | Self::Postgres | Self::Sqlite)
    }

    /// Quote one identifier the way the engine expects.
    pub fn quote(self, name: &str) -> String {
        match self {
            Self::SqlServer => quote_bracket(name),
            Self::MySql | Self::MariaDb => quote_backtick(name),
            Self::Postgres | Self::Oracle | Self::Sqlite | Self::DuckDb => quote_double(name),
        }
    }

    /// `schema.table`, quoted; the schema is left out when it is empty.
    pub fn qualified(self, schema: &str, table: &str) -> String {
        if schema.is_empty() {
            self.quote(table)
        } else {
            format!("{}.{}", self.quote(schema), self.quote(table))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexDdl {
    pub schema: String,
    pub table: String,
    pub name: String,
    /// Key columns, in key order.
    pub columns: Vec<String>,
    pub include_columns: Vec<String>,
    pub unique: bool,
    pub nonclustered: bool,
    /// Key columns (by name, as they appear in `columns`) to mark `DESC`.
    pub descending_columns: Vec<String>,
    /// Key expressions written as-is after the plain key columns, for an
    /// expression (function-based) index. Never quoted.
    pub key_expressions: Vec<String>,
    /// Raw predicate for a partial / filtered index. Ignored where the engine
    /// has no such feature.
    pub filter: Option<String>,
    /// SQL Server: rebuild an index of the same name in one step
    /// (`WITH (DROP_EXISTING = ON)`). Ignored elsewhere.
    pub drop_existing: bool,
}

impl IndexDdl {
    /// SQL Server: `CREATE [UNIQUE ][NONCLUSTERED ]INDEX [n] ON [s].[t] ([c]) INCLUDE ([i]);` (bracket quoting, `]` doubled).
    /// Postgres: `CREATE [UNIQUE ]INDEX "n" ON "s"."t" ("c") INCLUDE ("i");` (double-quote quoting, `"` doubled; INCLUDE needs PG 11+ — always emitted, PG < 11 is unsupported).
    /// MySQL / MariaDB: `CREATE [UNIQUE ]INDEX \`n\` ON \`s\`.\`t\` (\`c\`);` — INCLUDE is not supported: append included columns as trailing key columns and add a `-- covering columns appended` comment line.
    /// Oracle / DuckDB: double-quote quoting, no INCLUDE (appended like MySQL).
    /// SQLite: double-quote quoting, table never schema-qualified, no INCLUDE.
    pub fn render(&self, dialect: IndexDdlDialect) -> String {
        let unique = if self.unique { "UNIQUE " } else { "" };
        let nonclustered = if self.nonclustered && dialect == IndexDdlDialect::SqlServer {
            "NONCLUSTERED "
        } else {
            ""
        };
        let mut keys: Vec<String> = self
            .columns
            .iter()
            .map(|column| self.key_column(dialect, column))
            .collect();
        keys.extend(self.key_expressions.iter().map(|expression| {
            // MySQL functional key parts need their own parentheses.
            if matches!(dialect, IndexDdlDialect::MySql) {
                format!("({expression})")
            } else {
                expression.clone()
            }
        }));
        let mut include = String::new();
        let mut comment = String::new();
        if !self.include_columns.is_empty() {
            if dialect.supports_include() {
                include = format!(
                    " INCLUDE ({})",
                    self.include_columns
                        .iter()
                        .map(|column| dialect.quote(column))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            } else {
                keys.extend(
                    self.include_columns
                        .iter()
                        .map(|column| dialect.quote(column)),
                );
                comment = "\n-- covering columns appended".to_string();
            }
        }
        let filter = match &self.filter {
            Some(filter) if dialect.supports_filter() && !filter.trim().is_empty() => {
                format!(" WHERE {}", filter.trim())
            }
            _ => String::new(),
        };
        let with = if self.drop_existing && dialect == IndexDdlDialect::SqlServer {
            " WITH (DROP_EXISTING = ON)"
        } else {
            ""
        };
        let table = match dialect {
            IndexDdlDialect::Sqlite => dialect.quote(&self.table),
            _ => dialect.qualified(&self.schema, &self.table),
        };
        format!(
            "CREATE {unique}{nonclustered}INDEX {} ON {table} ({}){include}{filter}{with};{comment}",
            dialect.quote(&self.name),
            keys.join(", "),
        )
    }

    fn key_column(&self, dialect: IndexDdlDialect, column: &str) -> String {
        let quoted = dialect.quote(column);
        if self
            .descending_columns
            .iter()
            .any(|name| name.eq_ignore_ascii_case(column))
        {
            format!("{quoted} DESC")
        } else {
            quoted
        }
    }
}

fn quote_bracket(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn quote_double(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quote_backtick(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> IndexDdl {
        IndexDdl {
            schema: "dbo".to_string(),
            table: "t".to_string(),
            name: "IX_a".to_string(),
            columns: vec!["a".to_string(), "b".to_string()],
            include_columns: vec!["c".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn sqlserver_matches_admin_designer_output() {
        assert_eq!(
            sample().render(IndexDdlDialect::SqlServer),
            "CREATE INDEX [IX_a] ON [dbo].[t] ([a], [b]) INCLUDE ([c]);"
        );
    }

    #[test]
    fn sqlserver_nonclustered_unique() {
        let mut ddl = sample();
        ddl.unique = true;
        ddl.nonclustered = true;
        ddl.include_columns.clear();
        assert_eq!(
            ddl.render(IndexDdlDialect::SqlServer),
            "CREATE UNIQUE NONCLUSTERED INDEX [IX_a] ON [dbo].[t] ([a], [b]);"
        );
    }

    #[test]
    fn postgres_quotes_and_include() {
        assert_eq!(
            sample().render(IndexDdlDialect::Postgres),
            "CREATE INDEX \"IX_a\" ON \"dbo\".\"t\" (\"a\", \"b\") INCLUDE (\"c\");"
        );
        let mut ddl = sample();
        ddl.name = "ix\"x".to_string();
        assert!(
            ddl.render(IndexDdlDialect::Postgres)
                .contains("\"ix\"\"x\"")
        );
    }

    #[test]
    fn mysql_appends_includes_with_comment() {
        assert_eq!(
            sample().render(IndexDdlDialect::MySql),
            "CREATE INDEX `IX_a` ON `dbo`.`t` (`a`, `b`, `c`);\n-- covering columns appended"
        );
        let mut ddl = sample();
        ddl.include_columns.clear();
        assert_eq!(
            ddl.render(IndexDdlDialect::MySql),
            "CREATE INDEX `IX_a` ON `dbo`.`t` (`a`, `b`);"
        );
    }

    #[test]
    fn bracket_escaping() {
        let ddl = IndexDdl {
            schema: "d]b".to_string(),
            table: "t]x".to_string(),
            name: "IX]a".to_string(),
            columns: vec!["a]b".to_string()],
            ..Default::default()
        };
        assert_eq!(
            ddl.render(IndexDdlDialect::SqlServer),
            "CREATE INDEX [IX]]a] ON [d]]b].[t]]x] ([a]]b]);"
        );
    }

    #[test]
    fn mariadb_oracle_duckdb_append_includes_as_keys() {
        assert_eq!(
            sample().render(IndexDdlDialect::MariaDb),
            "CREATE INDEX `IX_a` ON `dbo`.`t` (`a`, `b`, `c`);\n-- covering columns appended"
        );
        assert_eq!(
            sample().render(IndexDdlDialect::Oracle),
            "CREATE INDEX \"IX_a\" ON \"dbo\".\"t\" (\"a\", \"b\", \"c\");\n-- covering columns appended"
        );
        assert_eq!(
            sample().render(IndexDdlDialect::DuckDb),
            "CREATE INDEX \"IX_a\" ON \"dbo\".\"t\" (\"a\", \"b\", \"c\");\n-- covering columns appended"
        );
    }

    #[test]
    fn sqlite_never_qualifies_the_table_and_keeps_a_partial_filter() {
        let mut ddl = sample();
        ddl.include_columns.clear();
        ddl.filter = Some("b IS NOT NULL".to_string());
        assert_eq!(
            ddl.render(IndexDdlDialect::Sqlite),
            "CREATE INDEX \"IX_a\" ON \"t\" (\"a\", \"b\") WHERE b IS NOT NULL;"
        );
        // MySQL has no partial indexes: the filter is dropped, not emitted.
        assert_eq!(
            ddl.render(IndexDdlDialect::MySql),
            "CREATE INDEX `IX_a` ON `dbo`.`t` (`a`, `b`);"
        );
    }

    #[test]
    fn descending_keys_expressions_and_drop_existing() {
        let mut ddl = sample();
        ddl.descending_columns = vec!["B".to_string()];
        ddl.drop_existing = true;
        assert_eq!(
            ddl.render(IndexDdlDialect::SqlServer),
            "CREATE INDEX [IX_a] ON [dbo].[t] ([a], [b] DESC) INCLUDE ([c]) WITH (DROP_EXISTING = ON);"
        );
        assert_eq!(
            ddl.render(IndexDdlDialect::Postgres),
            "CREATE INDEX \"IX_a\" ON \"dbo\".\"t\" (\"a\", \"b\" DESC) INCLUDE (\"c\");"
        );
        let expression = IndexDdl {
            table: "customers".to_string(),
            name: "ix_customers_lower_email".to_string(),
            key_expressions: vec!["lower(email)".to_string()],
            ..Default::default()
        };
        assert_eq!(
            expression.render(IndexDdlDialect::Postgres),
            "CREATE INDEX \"ix_customers_lower_email\" ON \"customers\" (lower(email));"
        );
        assert_eq!(
            expression.render(IndexDdlDialect::MySql),
            "CREATE INDEX `ix_customers_lower_email` ON `customers` ((lower(email)));"
        );
    }

    #[test]
    fn plan_dialects_map_and_clickhouse_has_none() {
        assert_eq!(
            IndexDdlDialect::for_plan(PlanDialect::Oracle),
            Some(IndexDdlDialect::Oracle)
        );
        assert_eq!(IndexDdlDialect::for_plan(PlanDialect::ClickHouse), None);
        assert!(IndexDdlDialect::Postgres.supports_include());
        assert!(!IndexDdlDialect::Sqlite.supports_include());
    }
}
