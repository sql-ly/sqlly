//! Shared `CREATE INDEX` generator for findings, the index designer, and
//! copy-to-editor actions.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexDdlDialect {
    SqlServer,
    Postgres,
    MySql,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexDdl {
    pub schema: String,
    pub table: String,
    pub name: String,
    pub columns: Vec<String>,
    pub include_columns: Vec<String>,
    pub unique: bool,
    pub nonclustered: bool,
}

impl IndexDdl {
    /// SQL Server: `CREATE [UNIQUE ][NONCLUSTERED ]INDEX [n] ON [s].[t] ([c]) INCLUDE ([i]);` (bracket quoting, `]` doubled).
    /// Postgres: `CREATE [UNIQUE ]INDEX "n" ON "s"."t" ("c") INCLUDE ("i");` (double-quote quoting, `"` doubled; INCLUDE needs PG 11+ — always emitted, PG < 11 is unsupported).
    /// MySQL: `CREATE [UNIQUE ]INDEX \`n\` ON \`s\`.\`t\` (\`c\`);` — INCLUDE is not supported: append included columns as trailing key columns and add a `-- covering columns appended` comment line.
    pub fn render(&self, dialect: IndexDdlDialect) -> String {
        match dialect {
            IndexDdlDialect::SqlServer => self.render_sql_server(),
            IndexDdlDialect::Postgres => self.render_postgres(),
            IndexDdlDialect::MySql => self.render_mysql(),
        }
    }

    fn render_sql_server(&self) -> String {
        let unique = if self.unique { "UNIQUE " } else { "" };
        let nonclustered = if self.nonclustered {
            "NONCLUSTERED "
        } else {
            ""
        };
        let columns = join_quoted(&self.columns, quote_bracket);
        let include = if self.include_columns.is_empty() {
            String::new()
        } else {
            format!(
                " INCLUDE ({})",
                join_quoted(&self.include_columns, quote_bracket)
            )
        };
        format!(
            "CREATE {unique}{nonclustered}INDEX {} ON {} ({columns}){include};",
            quote_bracket(&self.name),
            qualified(dialect_schema_table(
                &self.schema,
                &self.table,
                quote_bracket
            )),
        )
    }

    fn render_postgres(&self) -> String {
        let unique = if self.unique { "UNIQUE " } else { "" };
        let columns = join_quoted(&self.columns, quote_double);
        let include = if self.include_columns.is_empty() {
            String::new()
        } else {
            format!(
                " INCLUDE ({})",
                join_quoted(&self.include_columns, quote_double)
            )
        };
        format!(
            "CREATE {unique}INDEX {} ON {} ({columns}){include};",
            quote_double(&self.name),
            qualified(dialect_schema_table(
                &self.schema,
                &self.table,
                quote_double
            )),
        )
    }

    fn render_mysql(&self) -> String {
        let unique = if self.unique { "UNIQUE " } else { "" };
        let mut key_columns = self.columns.clone();
        let comment = if self.include_columns.is_empty() {
            String::new()
        } else {
            key_columns.extend(self.include_columns.iter().cloned());
            "\n-- covering columns appended".to_string()
        };
        let columns = join_quoted(&key_columns, quote_backtick);
        format!(
            "CREATE {unique}INDEX {} ON {} ({columns});{comment}",
            quote_backtick(&self.name),
            qualified(dialect_schema_table(
                &self.schema,
                &self.table,
                quote_backtick
            )),
        )
    }
}

fn dialect_schema_table<'a>(
    schema: &'a str,
    table: &'a str,
    quote: fn(&str) -> String,
) -> (Option<String>, String) {
    let quoted_table = quote(table);
    if schema.is_empty() {
        (None, quoted_table)
    } else {
        (Some(quote(schema)), quoted_table)
    }
}

fn qualified(parts: (Option<String>, String)) -> String {
    match parts {
        (Some(schema), table) => format!("{schema}.{table}"),
        (None, table) => table,
    }
}

fn join_quoted(names: &[String], quote: fn(&str) -> String) -> String {
    names
        .iter()
        .map(|name| quote(name))
        .collect::<Vec<_>>()
        .join(", ")
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
            unique: false,
            nonclustered: false,
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
            include_columns: vec![],
            unique: false,
            nonclustered: false,
        };
        assert_eq!(
            ddl.render(IndexDdlDialect::SqlServer),
            "CREATE INDEX [IX]]a] ON [d]]b].[t]]x] ([a]]b]);"
        );
    }
}
