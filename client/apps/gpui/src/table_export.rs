//! Pure planning for batch table exports — SQL targets and output paths.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use sqlly_lang::sqlmodel::DatabaseType;

use crate::export_options::{ExportOptions, PatternContext, filename_from_pattern, gz_path};
use crate::results::ExportFormat;

#[derive(Debug, Clone)]
pub struct TableExportPlan {
    pub database: String,
    pub folder: PathBuf,
    pub pattern: String,
    pub format: ExportFormat,
    pub options: ExportOptions,
    pub tables: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableExportTarget {
    pub schema: String,
    pub table: String,
    pub sql: String,
    pub path: PathBuf,
}

fn select_star_sql(db: DatabaseType, schema: &str, table: &str) -> String {
    let q = match schema.is_empty() {
        true => db.quote_identifier(table),
        false => format!(
            "{}.{}",
            db.quote_identifier(schema),
            db.quote_identifier(table)
        ),
    };
    format!("SELECT * FROM {q}")
}

fn path_with_collision_suffix(path: &Path, n: usize) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("export");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let ext = path.extension().and_then(|e| e.to_str());
    let file = match ext {
        Some(ext) => format!("{stem}_{n}.{ext}"),
        None => format!("{stem}_{n}"),
    };
    parent.join(file)
}

/// One target per table: `SELECT *` with engine quoting and a pattern-rendered
/// path under `folder`, `.gz` appended when `options.gzip`. Duplicate paths
/// get `_2`, `_3`, …
pub fn plan_targets(
    db: DatabaseType,
    plan: &TableExportPlan,
    date: &str,
) -> Vec<TableExportTarget> {
    let ext = plan.format.extension();
    let mut used = HashSet::new();
    let mut targets = Vec::with_capacity(plan.tables.len());
    for (schema, table) in &plan.tables {
        let ctx = PatternContext {
            schema,
            table,
            date,
            format: ext,
        };
        let mut path = plan.folder.join(filename_from_pattern(&plan.pattern, &ctx));
        if plan.options.gzip {
            path = gz_path(&path, true);
        }
        let mut candidate = path.clone();
        let mut n = 2usize;
        while used.contains(&candidate) {
            candidate = path_with_collision_suffix(&path, n);
            n += 1;
        }
        used.insert(candidate.clone());
        targets.push(TableExportTarget {
            schema: schema.clone(),
            table: table.clone(),
            sql: select_star_sql(db, schema, table),
            path: candidate,
        });
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan(db_tables: Vec<(String, String)>, pattern: &str, gzip: bool) -> TableExportPlan {
        TableExportPlan {
            database: "db".into(),
            folder: PathBuf::from("/out"),
            pattern: pattern.to_string(),
            format: ExportFormat::Csv,
            options: ExportOptions {
                gzip,
                ..ExportOptions::default_for(ExportFormat::Csv)
            },
            tables: db_tables,
        }
    }

    #[test]
    fn postgres_quoting() {
        let plan = sample_plan(
            vec![("public".into(), "orders".into())],
            DEFAULT_PATTERN,
            false,
        );
        let targets = plan_targets(DatabaseType::Postgres, &plan, "20260830");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].sql, r#"SELECT * FROM "public"."orders""#);
    }

    #[test]
    fn sql_server_quoting() {
        let plan = sample_plan(
            vec![("dbo".into(), "Orders".into())],
            DEFAULT_PATTERN,
            false,
        );
        let targets = plan_targets(DatabaseType::SqlServer, &plan, "20260830");
        assert_eq!(targets[0].sql, "SELECT * FROM [dbo].[Orders]");
    }

    #[test]
    fn duplicate_collision_suffix() {
        let plan = sample_plan(
            vec![("dbo".into(), "t".into()), ("dbo".into(), "t".into())],
            "{table}.{format}",
            false,
        );
        let targets = plan_targets(DatabaseType::SqlServer, &plan, "20260830");
        assert_eq!(targets[0].path, PathBuf::from("/out/t.csv"));
        assert_eq!(targets[1].path, PathBuf::from("/out/t_2.csv"));
    }

    #[test]
    fn gzip_suffix_on_path() {
        let plan = sample_plan(vec![("dbo".into(), "t".into())], DEFAULT_PATTERN, true);
        let targets = plan_targets(DatabaseType::SqlServer, &plan, "20260830");
        assert_eq!(targets[0].path, PathBuf::from("/out/dbo_t_20260830.csv.gz"));
    }

    #[test]
    fn pattern_without_table_still_distinct() {
        let plan = sample_plan(
            vec![("a".into(), "one".into()), ("b".into(), "two".into())],
            "{schema}_{date}.{format}",
            false,
        );
        let targets = plan_targets(DatabaseType::SqlServer, &plan, "20260830");
        assert_eq!(targets[0].path, PathBuf::from("/out/a_20260830.csv"));
        assert_eq!(targets[1].path, PathBuf::from("/out/b_20260830.csv"));
        assert_ne!(targets[0].path, targets[1].path);
    }

    use crate::export_options::DEFAULT_PATTERN;
}
