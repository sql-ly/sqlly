//! Generic SQL-driven catalog collector: one [`CatalogCollector`] serving
//! every engine with dialect-dispatched metadata SQL (ADR-011 delivery phases
//! 2-4 — SQL Server, PostgreSQL, MySQL/MariaDB; the DDL-parsing SQLite
//! collector in [`crate::modelengine::sqlite_driver`] remains the phase-1 reference and
//! also works through this path).
//!
//! The catalog queries come from `crate::metadata::sql_for_dialect` — the SAME
//! generator the metadata service uses — so explorer listings and the model
//! engine can never drift apart on what a schema looks like. Driver crates
//! contribute only a [`PooledDriver`](crate::modelengine::pool::PooledDriver) that runs SQL
//! and returns string rows; everything dialect-specific lives in the shared
//! SQL layer, and everything model-specific lives here.

use crate::metadata::{MetadataDialect, MetadataQuery, sql_for_dialect};
use crate::modelengine::collect::{CatalogCollector, CollectUnit};
use crate::modelengine::pool::{ConnectionPool, PoolError};
use async_trait::async_trait;
use sqlly_lang::sqlmodel::{
    ColumnDef, Dialect, Fidelity, ForeignKeyDef, KeyDef, ObjectBody, ObjectFragment, ObjectKind,
    ObjectRef, ParamDef, Provenance, RoutineBody, SqlName, TableBody, ViewBody,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Catalog collection for any engine family with dialect-dispatched metadata
/// SQL. Pairs with that engine's [`PooledDriver`](crate::modelengine::pool::PooledDriver).
pub struct SqlCatalogCollector {
    metadata_dialect: MetadataDialect,
    dialect: Dialect,
}

impl SqlCatalogCollector {
    /// Collector for a database type, or `None` when the type has no
    /// metadata dialect (no native driver yet).
    pub fn for_database_type(
        db: sqlly_lang::sqlmodel::DatabaseType,
    ) -> Option<SqlCatalogCollector> {
        Some(SqlCatalogCollector {
            metadata_dialect: MetadataDialect::for_database_type(db)?,
            dialect: db.family(),
        })
    }
}

type StringRows = Vec<Vec<Option<String>>>;

/// Run one metadata query through a pooled connection.
async fn run(
    pool: &Arc<ConnectionPool>,
    metadata_dialect: MetadataDialect,
    query: &MetadataQuery,
) -> Result<StringRows, PoolError> {
    let sql = sql_for_dialect(query, metadata_dialect).map_err(|e| e.to_string())?;
    let mut conn = pool.acquire().await?;
    conn.query(&sql, &[]).await
}

fn cell(row: &[Option<String>], i: usize) -> Option<&str> {
    row.get(i).and_then(|c| c.as_deref())
}

/// Lower-cased join key for correlating bulk listings, mirroring the
/// metadata-service snapshot builder.
fn key(schema: &str, name: &str) -> (String, String) {
    (schema.to_ascii_lowercase(), name.to_ascii_lowercase())
}

fn object_ref(schema: &str, name: &str, dialect: Dialect) -> ObjectRef {
    ObjectRef {
        catalog: None,
        namespace: (!schema.is_empty()).then(|| SqlName::new(schema, dialect, false)),
        name: SqlName::new(name, dialect, false),
    }
}

fn column_defs(columns: &[String], dialect: Dialect) -> Vec<ColumnDef> {
    columns
        .iter()
        .enumerate()
        .map(|(i, name)| ColumnDef {
            name: SqlName::new(name, dialect, false),
            ty: None,
            nullable: None,
            default: None,
            identity: false,
            generated: None,
            collation: None,
            ordinal: i,
        })
        .collect()
}

#[async_trait]
impl CatalogCollector for SqlCatalogCollector {
    fn dialect(&self) -> Dialect {
        self.dialect
    }

    async fn plan(&self, _pool: &Arc<ConnectionPool>) -> Result<Vec<CollectUnit>, PoolError> {
        // Kind batches, like the SQLite reference collector. Region prefixes
        // are empty: a failed batch marks the whole source incomplete, which
        // is the conservative reading ("this source's model may be missing
        // objects") rather than pretending we know which names were lost.
        Ok(["tables", "views", "routines"]
            .into_iter()
            .map(|kind| CollectUnit {
                label: format!("{:?}:{kind}", self.metadata_dialect).to_ascii_lowercase(),
                region_prefix: String::new(),
            })
            .collect())
    }

    async fn collect(
        &self,
        pool: &Arc<ConnectionPool>,
        unit: &CollectUnit,
        profile: &str,
    ) -> Result<Vec<ObjectFragment>, PoolError> {
        let provenance = || Provenance::Database {
            profile: profile.to_string(),
        };
        let md = self.metadata_dialect;
        let dialect = self.dialect;

        if unit.label.ends_with("tables") {
            let tables = run(pool, md, &MetadataQuery::Tables { schema: None }).await?;
            let columns = run(pool, md, &MetadataQuery::AllColumns).await?;
            let pks = run(pool, md, &MetadataQuery::AllPrimaryKeys).await?;
            let fks = run(pool, md, &MetadataQuery::AllForeignKeys).await?;

            // (schema, object) → ordinal-ordered column names.
            let mut columns_by_object: HashMap<(String, String), Vec<String>> = HashMap::new();
            for row in &columns {
                if let (Some(schema), Some(object), Some(column)) =
                    (cell(row, 0), cell(row, 1), cell(row, 2))
                {
                    columns_by_object
                        .entry(key(schema, object))
                        .or_default()
                        .push(column.to_string());
                }
            }
            // (schema, table) → key-ordinal-ordered PK column names.
            let mut pk_by_table: HashMap<(String, String), Vec<String>> = HashMap::new();
            for row in &pks {
                if let (Some(schema), Some(table), Some(column)) =
                    (cell(row, 0), cell(row, 1), cell(row, 2))
                {
                    pk_by_table
                        .entry(key(schema, table))
                        .or_default()
                        .push(column.to_string());
                }
            }
            // (schema, table) → FK defs, grouped by constraint name so
            // composite keys regroup into one relationship.
            let mut fk_by_table: HashMap<(String, String), Vec<ForeignKeyDef>> = HashMap::new();
            {
                let mut last: Option<(String, (String, String))> = None;
                for row in &fks {
                    let (Some(cname), Some(fs), Some(ft), Some(fc), Some(ts), Some(tt), Some(tc)) = (
                        cell(row, 0),
                        cell(row, 1),
                        cell(row, 2),
                        cell(row, 3),
                        cell(row, 4),
                        cell(row, 5),
                        cell(row, 6),
                    ) else {
                        continue;
                    };
                    let table_key = key(fs, ft);
                    let same_constraint = last
                        .as_ref()
                        .is_some_and(|(name, table)| name == cname && *table == table_key);
                    if same_constraint {
                        if let Some(def) =
                            fk_by_table.get_mut(&table_key).and_then(|v| v.last_mut())
                        {
                            def.columns.push(SqlName::new(fc, dialect, false));
                            def.target_columns.push(SqlName::new(tc, dialect, false));
                        }
                    } else {
                        fk_by_table
                            .entry(table_key.clone())
                            .or_default()
                            .push(ForeignKeyDef {
                                name: Some(SqlName::new(cname, dialect, false)),
                                columns: vec![SqlName::new(fc, dialect, false)],
                                target: object_ref(ts, tt, dialect),
                                target_columns: vec![SqlName::new(tc, dialect, false)],
                            });
                        last = Some((cname.to_string(), table_key));
                    }
                }
            }

            let mut fragments = Vec::new();
            for row in &tables {
                let (Some(schema), Some(name)) = (cell(row, 0), cell(row, 1)) else {
                    continue;
                };
                let table_key = key(schema, name);
                let columns = columns_by_object.remove(&table_key).unwrap_or_default();
                let primary_key = pk_by_table.remove(&table_key).unwrap_or_default();
                let foreign_keys = fk_by_table.remove(&table_key).unwrap_or_default();
                let references = foreign_keys.iter().map(|fk| fk.target.clone()).collect();
                fragments.push(ObjectFragment {
                    reference: object_ref(schema, name, dialect),
                    kind: ObjectKind::Table,
                    dialect,
                    body: ObjectBody::Table(TableBody {
                        columns: column_defs(&columns, dialect),
                        primary_key: (!primary_key.is_empty()).then(|| KeyDef {
                            name: None,
                            columns: primary_key
                                .iter()
                                .map(|c| SqlName::new(c, dialect, false))
                                .collect(),
                        }),
                        unique_keys: Vec::new(),
                        foreign_keys,
                        checks: Vec::new(),
                        options: std::collections::BTreeMap::new(),
                    }),
                    provenance: provenance(),
                    fidelity: Fidelity::Exact,
                    references,
                });
            }
            return Ok(fragments);
        }

        if unit.label.ends_with("views") {
            let views = run(pool, md, &MetadataQuery::Views { schema: None }).await?;
            let columns = run(pool, md, &MetadataQuery::AllColumns).await?;
            let mut columns_by_object: HashMap<(String, String), Vec<String>> = HashMap::new();
            for row in &columns {
                if let (Some(schema), Some(object), Some(column)) =
                    (cell(row, 0), cell(row, 1), cell(row, 2))
                {
                    columns_by_object
                        .entry(key(schema, object))
                        .or_default()
                        .push(column.to_string());
                }
            }
            let mut fragments = Vec::new();
            for row in &views {
                let (Some(schema), Some(name)) = (cell(row, 0), cell(row, 1)) else {
                    continue;
                };
                let columns = columns_by_object
                    .remove(&key(schema, name))
                    .unwrap_or_default();
                fragments.push(ObjectFragment {
                    reference: object_ref(schema, name, dialect),
                    kind: ObjectKind::View,
                    dialect,
                    body: ObjectBody::View(ViewBody {
                        columns: column_defs(&columns, dialect),
                        definition: None,
                    }),
                    provenance: provenance(),
                    fidelity: Fidelity::Exact,
                    references: Vec::new(),
                });
            }
            return Ok(fragments);
        }

        // Routines: symbols with kinds; parameters arrive through the lazy
        // per-routine metadata path, exactly as the snapshot builder does.
        let routines = run(
            pool,
            md,
            &MetadataQuery::ProgrammableObjects { schema: None },
        )
        .await?;
        let mut fragments = Vec::new();
        for row in &routines {
            let (Some(schema), Some(name)) = (cell(row, 0), cell(row, 1)) else {
                continue;
            };
            let kind = match cell(row, 2).unwrap_or_default() {
                "P" => ObjectKind::Procedure,
                "IF" | "TF" => ObjectKind::TableFunction,
                "TR" => ObjectKind::Trigger,
                _ => ObjectKind::ScalarFunction,
            };
            fragments.push(ObjectFragment {
                reference: object_ref(schema, name, dialect),
                kind,
                dialect,
                body: ObjectBody::Routine(RoutineBody {
                    params: Vec::<ParamDef>::new(),
                    returns: None,
                    language: None,
                    body_text: None,
                }),
                provenance: provenance(),
                fidelity: Fidelity::Exact,
                references: Vec::new(),
            });
        }
        Ok(fragments)
    }
}
