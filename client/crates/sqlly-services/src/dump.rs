//! Streaming database dump writer (native targets only).

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use chrono::Utc;
use sqlly_core::{CellValue, ObjectKind, StudioError, StudioResult};
use sqlly_intel::intellisense::{ClickedRole, NavigationTarget, RoutineKind};
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};

use crate::ddl::{DdlOptions, fetch_ddl_document_with};
use crate::dump_render::{
    DumpContent, DumpLayout, DumpOptions, batch_separator, drop_statement, dump_header,
    identity_guard, order_tables, render_insert_batch, script_definition, session_epilogue,
    session_prelude, table_key,
};
use crate::metadata::{BulkColumnRow, profile_with_db};
use crate::query::QueryEvent;
use crate::services::Services;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpPhase {
    Enumerating,
    Structure,
    Data,
    Finishing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpProgress {
    pub phase: DumpPhase,
    pub object: String,
    pub objects_done: usize,
    pub objects_total: usize,
    pub rows_written: u64,
    pub bytes_written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpReport {
    pub files: Vec<PathBuf>,
    pub rows_written: u64,
    pub bytes_written: u64,
    pub warnings: Vec<sqlly_core::DisplayMessage>,
    pub elapsed_ms: u128,
    pub cancelled: bool,
}

#[derive(Clone)]
struct DumpObject {
    schema: String,
    name: String,
    kind: ObjectKind,
    label: String,
}

#[derive(Clone)]
struct TableDumpInfo {
    schema: String,
    name: String,
    label: String,
    columns: Vec<String>,
    identity_columns: Vec<String>,
}

struct DumpWriter {
    db: DatabaseType,
    layout: DumpLayout,
    output: PathBuf,
    files: Vec<PathBuf>,
    writer: Option<BufWriter<File>>,
    bytes_written: u64,
    manifest_lines: Vec<String>,
    sequence: usize,
}

impl DumpWriter {
    fn new(db: DatabaseType, layout: DumpLayout, output: PathBuf) -> StudioResult<Self> {
        if layout == DumpLayout::FilePerObject {
            fs::create_dir_all(&output)?;
            fs::create_dir_all(output.join("structure"))?;
            fs::create_dir_all(output.join("data"))?;
        } else if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(Self {
            db,
            layout,
            output,
            files: Vec::new(),
            writer: None,
            bytes_written: 0,
            manifest_lines: Vec::new(),
            sequence: 0,
        })
    }

    fn open_single(&mut self) -> StudioResult<()> {
        if self.writer.is_some() {
            return Ok(());
        }
        let file = File::create(&self.output)?;
        self.files.push(self.output.clone());
        self.writer = Some(BufWriter::new(file));
        Ok(())
    }

    fn open_part(&mut self, subdir: &str, name: &str) -> StudioResult<()> {
        self.sequence += 1;
        let file_name = format!("{:03}_{}.sql", self.sequence, sanitize_filename(name));
        let path = self.output.join(subdir).join(file_name);
        let file = File::create(&path)?;
        self.files.push(path.clone());
        self.manifest_lines.push(format!(":r {}", path.display()));
        self.writer = Some(BufWriter::new(file));
        Ok(())
    }

    fn write_str(&mut self, text: &str) -> StudioResult<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.write_all(text.as_bytes())?;
            self.bytes_written += text.len() as u64;
        }
        Ok(())
    }

    fn flush(&mut self) -> StudioResult<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }
        Ok(())
    }

    fn close_current(&mut self) -> StudioResult<()> {
        if let Some(mut writer) = self.writer.take() {
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }
        Ok(())
    }

    fn write_header_file(&mut self, header: &str) -> StudioResult<()> {
        match self.layout {
            DumpLayout::SingleFile => {
                self.open_single()?;
                self.write_str(header)?;
                self.write_str(&session_prelude(self.db))?;
            }
            DumpLayout::FilePerObject => {
                let path = self.output.join("00_header.sql");
                let mut file = BufWriter::new(File::create(&path)?);
                file.write_all(header.as_bytes())?;
                file.write_all(session_prelude(self.db).as_bytes())?;
                file.flush()?;
                file.get_ref().sync_all()?;
                self.files.push(path.clone());
                self.manifest_lines
                    .insert(0, format!(":r {}", path.display()));
                self.bytes_written += (header.len() + session_prelude(self.db).len()) as u64;
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> StudioResult<()> {
        let epilogue = session_epilogue(self.db);
        match self.layout {
            DumpLayout::SingleFile => {
                self.write_str(&epilogue)?;
                self.close_current()?;
            }
            DumpLayout::FilePerObject => {
                let path = self.output.join("99_epilogue.sql");
                let mut file = BufWriter::new(File::create(&path)?);
                file.write_all(epilogue.as_bytes())?;
                file.flush()?;
                file.get_ref().sync_all()?;
                self.files.push(path.clone());
                self.manifest_lines.push(format!(":r {}", path.display()));
                let manifest_path = self.output.join("manifest.sql");
                let manifest = self.manifest_lines.join("\n");
                fs::write(&manifest_path, format!("{manifest}\n"))?;
                self.files.push(manifest_path);
            }
        }
        Ok(())
    }
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Reorder `objects` to follow `order` (qualified labels): named objects
/// first, in the list's order; the rest after, keeping their current order.
/// A stable sort, so objects sharing a label (overloaded routines) keep
/// their relative order.
fn apply_structure_order(objects: &mut [DumpObject], order: &[String]) {
    objects.sort_by_key(|obj| {
        order
            .iter()
            .position(|label| label == &obj.label)
            .unwrap_or(usize::MAX)
    });
}

fn schema_allowed(schema: &str, opts: &DumpOptions) -> bool {
    match &opts.schemas {
        None => true,
        Some(list) => list.iter().any(|s| s == schema),
    }
}

fn navigation_target(obj: &DumpObject) -> NavigationTarget {
    match obj.kind {
        ObjectKind::View => NavigationTarget::Table {
            catalog: None,
            schema: obj.schema.clone(),
            name: obj.name.clone(),
            clicked: ClickedRole::View,
        },
        ObjectKind::StoredProcedure => NavigationTarget::Routine {
            catalog: None,
            schema: obj.schema.clone(),
            name: obj.name.clone(),
            kind: RoutineKind::Procedure,
            clicked: ClickedRole::Procedure,
        },
        ObjectKind::ScalarFunction | ObjectKind::TableFunction | ObjectKind::AggregateFunction => {
            NavigationTarget::Routine {
                catalog: None,
                schema: obj.schema.clone(),
                name: obj.name.clone(),
                kind: RoutineKind::Function,
                clicked: ClickedRole::Function,
            }
        }
        _ => NavigationTarget::Table {
            catalog: None,
            schema: obj.schema.clone(),
            name: obj.name.clone(),
            clicked: ClickedRole::Table,
        },
    }
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

fn cyclic_tables_from_warnings(warnings: &[sqlly_core::DisplayMessage]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for warning in warnings {
        if !warning.contains("cycle") {
            continue;
        }
        for token in warning.split(|c: char| !c.is_alphanumeric() && c != '.' && c != '_') {
            if token.contains('.') {
                out.insert(token.to_string());
            }
        }
    }
    out
}

fn ncheck_sql(db: DatabaseType, schema: &str, table: &str, check: bool) -> Option<String> {
    if db.connection_kind() != ConnectionKind::Tds {
        return None;
    }
    let q = match schema.is_empty() {
        true => db.quote_identifier(table),
        false => format!(
            "{}.{}",
            db.quote_identifier(schema),
            db.quote_identifier(table)
        ),
    };
    Some(if check {
        format!("ALTER TABLE {q} WITH CHECK CHECK CONSTRAINT ALL;")
    } else {
        format!("ALTER TABLE {q} NOCHECK CONSTRAINT ALL;")
    })
}

pub async fn run_database_dump(
    services: &Services,
    profile: &sqlly_core::ConnectionProfile,
    database: &str,
    output: &Path,
    opts: &DumpOptions,
    cancel: Arc<AtomicBool>,
    progress: &mut dyn FnMut(DumpProgress),
) -> StudioResult<DumpReport> {
    let started = Instant::now();
    let db = profile.database_type;
    let batch_size = opts.effective_batch_size(db);
    let mut warnings = Vec::new();
    let mut rows_written = 0u64;

    progress(DumpProgress {
        phase: DumpPhase::Enumerating,
        object: String::new(),
        objects_done: 0,
        objects_total: 0,
        rows_written: 0,
        bytes_written: 0,
    });

    let schemas = services
        .metadata
        .list_schemas(profile, database)
        .await?
        .into_iter()
        .filter(|s| schema_allowed(s, opts))
        .collect::<Vec<_>>();

    let mut tables = Vec::new();
    let mut views = Vec::new();
    let mut routines = Vec::new();

    for schema in &schemas {
        for node in services
            .metadata
            .list_tables(profile, database, Some(schema))
            .await?
        {
            let schema_name = node.schema.clone().unwrap_or_else(|| schema.clone());
            tables.push(DumpObject {
                schema: schema_name.clone(),
                name: node.name.clone(),
                kind: ObjectKind::Table,
                label: table_key(&schema_name, &node.name),
            });
        }
        if opts.include_views {
            for node in services
                .metadata
                .list_views(profile, database, Some(schema))
                .await?
            {
                let schema_name = node.schema.clone().unwrap_or_else(|| schema.clone());
                views.push(DumpObject {
                    schema: schema_name.clone(),
                    name: node.name.clone(),
                    kind: ObjectKind::View,
                    label: table_key(&schema_name, &node.name),
                });
            }
        }
        if opts.include_routines {
            for node in services
                .metadata
                .list_programmable_objects(profile, database, Some(schema))
                .await?
            {
                let schema_name = node.schema.clone().unwrap_or_else(|| schema.clone());
                routines.push(DumpObject {
                    schema: schema_name.clone(),
                    name: node.name.clone(),
                    kind: node.kind,
                    label: table_key(&schema_name, &node.name),
                });
            }
        }
    }

    // An explicit object scope (e.g. a Deploy Cart group) filters the
    // enumerated objects by qualified label before anything is ordered or read.
    if let Some(objects) = &opts.objects {
        let in_scope = |obj: &DumpObject| objects.iter().any(|label| label == &obj.label);
        tables.retain(in_scope);
        views.retain(in_scope);
        routines.retain(in_scope);
    }

    let fks = match services.metadata.list_foreign_keys(profile, database).await {
        Ok(fks) => fks,
        Err(StudioError::NotImplemented(_)) => {
            warnings.push(sqlly_core::DisplayMessage::new(
                "services.dump.warnings.foreign_keys_unavailable",
                "Foreign keys unavailable — table data order may not respect dependencies",
            ));
            Vec::new()
        }
        Err(err) => return Err(err),
    };

    let table_pairs: Vec<(String, String)> = tables
        .iter()
        .map(|t| (t.schema.clone(), t.name.clone()))
        .collect();
    let table_order = order_tables(&table_pairs, &fks);
    warnings.extend(table_order.warnings.clone());

    let columns = load_columns(services, profile, database, &tables, &mut warnings).await?;
    let duckdb_sequences =
        if opts.include_sequences && db.connection_kind() == ConnectionKind::DuckDbFile {
            load_duckdb_sequences(services, profile, database, &mut warnings).await
        } else {
            HashMap::new()
        };
    let mut table_infos: Vec<TableDumpInfo> = Vec::new();
    for key in &table_order.order {
        let Some(obj) = tables.iter().find(|t| t.label == *key) else {
            continue;
        };
        let cols = columns.get(key).cloned().unwrap_or_default();
        let col_names = cols.iter().map(|c| c.column.clone()).collect();
        let identity_columns = cols
            .iter()
            .filter(|c| c.is_identity)
            .map(|c| c.column.clone())
            .collect();
        table_infos.push(TableDumpInfo {
            schema: obj.schema.clone(),
            name: obj.name.clone(),
            label: obj.label.clone(),
            columns: col_names,
            identity_columns,
        });
    }

    let trigger_order = if db.connection_kind() == ConnectionKind::MySqlWire
        && routines.iter().any(|r| r.kind == ObjectKind::Trigger)
    {
        load_mysql_trigger_order(services, profile, database, &mut warnings).await
    } else {
        HashMap::new()
    };
    let structure_objects = {
        let mut ordered = Vec::new();
        for key in &table_order.order {
            if let Some(obj) = tables.iter().find(|t| t.label == *key) {
                ordered.push(obj.clone());
            }
        }
        views.sort_by(|a, b| a.label.cmp(&b.label));
        routines.sort_by(|a, b| a.label.cmp(&b.label));
        sort_triggers_by_firing_order(&mut routines, &trigger_order);
        ordered.extend(views.clone());
        ordered.extend(routines.clone());
        if let Some(order) = &opts.structure_order {
            apply_structure_order(&mut ordered, order);
        }
        ordered
    };

    // `data_objects` narrows which in-scope tables write INSERT batches;
    // structure above still covers the whole `objects` scope.
    let data_tables: Vec<TableDumpInfo> = match &opts.data_objects {
        Some(list) => table_infos
            .into_iter()
            .filter(|t| list.iter().any(|label| label == &t.label))
            .collect(),
        None => table_infos,
    };
    let objects_total = match opts.content {
        DumpContent::StructureOnly => structure_objects.len(),
        DumpContent::DataOnly => data_tables.len(),
        DumpContent::StructureAndData => structure_objects.len() + data_tables.len(),
    };

    let mut writer = DumpWriter::new(db, opts.layout, output.to_path_buf())?;
    let header = dump_header(db, database, Utc::now(), opts);
    writer.write_header_file(&header)?;

    let cyclic = cyclic_tables_from_warnings(&warnings);

    if matches!(
        opts.content,
        DumpContent::StructureOnly | DumpContent::StructureAndData
    ) {
        for (index, obj) in structure_objects.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                writer.write_str("-- CANCELLED\n")?;
                writer.flush()?;
                return Ok(DumpReport {
                    files: writer.files,
                    rows_written,
                    bytes_written: writer.bytes_written,
                    warnings,
                    elapsed_ms: started.elapsed().as_millis(),
                    cancelled: true,
                });
            }
            progress(DumpProgress {
                phase: DumpPhase::Structure,
                object: obj.label.clone(),
                objects_done: index,
                objects_total,
                rows_written,
                bytes_written: writer.bytes_written,
            });

            if opts.layout == DumpLayout::FilePerObject {
                writer.close_current()?;
                writer.open_part("structure", &obj.label)?;
            } else {
                writer.open_single()?;
            }

            if opts.drop_if_exists {
                let drop = drop_statement(db, obj.kind.clone(), Some(&obj.schema), &obj.name);
                writer.write_str(&drop)?;
                writer.write_str("\n")?;
                write_separator_to_writer(&mut writer, db)?;
            }

            let doc = fetch_ddl_document_with(
                services.queries.as_ref(),
                profile,
                database,
                &navigation_target(obj),
                DdlOptions {
                    include_privileges: opts.include_privileges,
                },
            )
            .await?;
            if let Some(err) = doc.error {
                warnings.push(
                    sqlly_core::DisplayMessage::new(
                        "services.dump.warnings.definition_unavailable",
                        format!("{}: {err}", obj.label),
                    )
                    .with_arg("object_name", obj.label.clone())
                    .with_arg("detail", err.clone()),
                );
                writer.write_str(&format!("-- SKIPPED: {err}\n"))?;
            } else if let Some(source) = doc.primary_source() {
                let text = script_definition(db, &obj.kind, &source.body);
                writer.write_str(&text)?;
                if !text.ends_with('\n') {
                    writer.write_str("\n")?;
                }
            }
            write_separator_to_writer(&mut writer, db)?;
        }
    }

    if matches!(
        opts.content,
        DumpContent::DataOnly | DumpContent::StructureAndData
    ) {
        for (index, table) in data_tables.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                writer.write_str("-- CANCELLED\n")?;
                writer.flush()?;
                return Ok(DumpReport {
                    files: writer.files,
                    rows_written,
                    bytes_written: writer.bytes_written,
                    warnings,
                    elapsed_ms: started.elapsed().as_millis(),
                    cancelled: true,
                });
            }

            progress(DumpProgress {
                phase: DumpPhase::Data,
                object: table.label.clone(),
                objects_done: structure_objects.len() + index,
                objects_total,
                rows_written,
                bytes_written: writer.bytes_written,
            });

            if opts.layout == DumpLayout::FilePerObject {
                writer.close_current()?;
                writer.open_part("data", &table.label)?;
            } else {
                writer.open_single()?;
            }

            if cyclic.contains(&table.label) {
                if let Some(sql) = ncheck_sql(db, &table.schema, &table.name, false) {
                    writer.write_str(&sql)?;
                    writer.write_str("\n")?;
                }
                writer.write_str(&format!(
                    "-- WARNING: circular foreign keys involving {}\n",
                    table.label
                ))?;
            }

            let qualified = match table.schema.is_empty() {
                true => db.quote_identifier(&table.name),
                false => format!(
                    "{}.{}",
                    db.quote_identifier(&table.schema),
                    db.quote_identifier(&table.name)
                ),
            };

            let sql = select_star_sql(db, &table.schema, &table.name);
            let handle = services
                .queries
                // Full values: INSERTs must carry every value whole, never the
                // 8 KiB preview a grid paints.
                .execute_full_values(&profile_with_db(profile, database), None, &sql, 256)
                .await?;
            let mut events = handle.events;
            let mut batch: Vec<Vec<CellValue>> = Vec::new();
            let mut identity_on = false;

            while let Some(event) = events.recv().await {
                match event {
                    QueryEvent::Rows { rows, .. } => {
                        if cancel.load(Ordering::Relaxed) {
                            let _ = services.queries.cancel(&handle.task_id).await;
                            writer.write_str("-- CANCELLED\n")?;
                            writer.flush()?;
                            return Ok(DumpReport {
                                files: writer.files,
                                rows_written,
                                bytes_written: writer.bytes_written,
                                warnings,
                                elapsed_ms: started.elapsed().as_millis(),
                                cancelled: true,
                            });
                        }
                        for row in rows {
                            batch.push(row);
                            if batch.len() >= batch_size {
                                flush_batch(
                                    &mut writer,
                                    db,
                                    table,
                                    &mut batch,
                                    &mut identity_on,
                                    &qualified,
                                    &mut rows_written,
                                )?;
                            }
                        }
                    }
                    QueryEvent::Error { message } => {
                        warnings.push(
                            sqlly_core::DisplayMessage::new(
                                "services.dump.warnings.table_data_failed",
                                format!("{} data: {message}", table.label),
                            )
                            .with_arg("table_name", table.label.clone())
                            .with_arg("detail", message),
                        );
                        break;
                    }
                    QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
                    _ => {}
                }
            }

            if !batch.is_empty() {
                flush_batch(
                    &mut writer,
                    db,
                    table,
                    &mut batch,
                    &mut identity_on,
                    &qualified,
                    &mut rows_written,
                )?;
            }

            if identity_on && let Some(off) = identity_guard(db, &qualified, false) {
                writer.write_str(&off)?;
                writer.write_str("\n")?;
            }

            if cyclic.contains(&table.label)
                && let Some(sql) = ncheck_sql(db, &table.schema, &table.name, true)
            {
                writer.write_str(&sql)?;
                writer.write_str("\n")?;
            }

            if opts.include_sequences && db.connection_kind() == ConnectionKind::PostgresWire {
                for col in &table.identity_columns {
                    let q = match table.schema.is_empty() {
                        true => db.quote_identifier(&table.name),
                        false => format!(
                            "{}.{}",
                            db.quote_identifier(&table.schema),
                            db.quote_identifier(&table.name)
                        ),
                    };
                    let col_q = db.quote_identifier(col);
                    let stmt = format!(
                        "SELECT setval(pg_get_serial_sequence('{q}', '{col}'), COALESCE(MAX({col_q}), 1)) FROM {q};\n"
                    );
                    writer.write_str(&stmt)?;
                }
            }
            if opts.include_sequences && db.connection_kind() == ConnectionKind::DuckDbFile {
                for col in &table.identity_columns {
                    let key = (table.schema.clone(), table.name.clone(), col.clone());
                    match duckdb_sequences.get(&key) {
                        Some(sequence) => {
                            let stmt = crate::dump_render::duckdb_sequence_advance_sql(
                                db, &qualified, col, sequence,
                            );
                            writer.write_str(&stmt)?;
                            writer.write_str("\n")?;
                        }
                        None => writer.write_str(&format!(
                            "-- {}.{col}: sequence name not found in the catalog; advance it by hand if new rows collide\n",
                            table.label
                        ))?,
                    }
                }
            }

            write_separator_to_writer(&mut writer, db)?;
        }
    }

    progress(DumpProgress {
        phase: DumpPhase::Finishing,
        object: String::new(),
        objects_done: objects_total,
        objects_total,
        rows_written,
        bytes_written: writer.bytes_written,
    });

    writer.finish()?;

    Ok(DumpReport {
        files: writer.files,
        rows_written,
        bytes_written: writer.bytes_written,
        warnings,
        elapsed_ms: started.elapsed().as_millis(),
        cancelled: false,
    })
}

fn write_separator_to_writer(writer: &mut DumpWriter, db: DatabaseType) -> StudioResult<()> {
    let sep = batch_separator(db);
    if sep.is_empty() {
        writer.write_str("\n")
    } else {
        writer.write_str(sep)?;
        writer.write_str("\n")
    }
}

fn flush_batch(
    writer: &mut DumpWriter,
    db: DatabaseType,
    table: &TableDumpInfo,
    batch: &mut Vec<Vec<CellValue>>,
    identity_on: &mut bool,
    qualified: &str,
    rows_written: &mut u64,
) -> StudioResult<()> {
    if batch.is_empty() {
        return Ok(());
    }
    if !table.identity_columns.is_empty()
        && !*identity_on
        && let Some(on) = identity_guard(db, qualified, true)
    {
        writer.write_str(&on)?;
        writer.write_str("\n")?;
        *identity_on = true;
    }
    let chunk: Vec<_> = std::mem::take(batch);
    *rows_written += chunk.len() as u64;
    let stmt = render_insert_batch(db, Some(&table.schema), &table.name, &table.columns, &chunk);
    writer.write_str(&stmt)?;
    writer.write_str("\n")?;
    Ok(())
}

/// Where a MySQL/MariaDB trigger fires: `(table, timing, event, action
/// order)`, keyed by the trigger's dump label.
type TriggerFiring = (String, String, String, i64);

/// Every MySQL/MariaDB trigger's firing position. Triggers sharing a table,
/// timing and event fire in ACTION_ORDER, and their definitions name the
/// trigger they `FOLLOWS`, so the dump must create them in that order (a
/// `FOLLOWS` naming a trigger not created yet fails). A failed read degrades
/// to a warning and the alphabetical order.
async fn load_mysql_trigger_order(
    services: &Services,
    profile: &sqlly_core::ConnectionProfile,
    database: &str,
    warnings: &mut Vec<sqlly_core::DisplayMessage>,
) -> HashMap<String, TriggerFiring> {
    const SQL: &str = "SELECT trigger_schema, trigger_name, event_object_table, action_timing, \
        event_manipulation, action_order FROM information_schema.triggers \
        WHERE trigger_schema = DATABASE()";
    let mut out = HashMap::new();
    match crate::ddl::collect_rows(
        services.queries.as_ref(),
        profile_with_db(profile, database),
        SQL,
    )
    .await
    {
        Ok(rows) => {
            for row in rows {
                let text = |i: usize| match row.get(i) {
                    Some(CellValue::String(s)) => s.clone(),
                    Some(CellValue::I64(n)) => n.to_string(),
                    _ => String::new(),
                };
                let order = text(5).parse().unwrap_or(0);
                out.insert(
                    table_key(&text(0), &text(1)),
                    (text(2), text(3), text(4), order),
                );
            }
        }
        Err(err) => warnings.push(
            sqlly_core::DisplayMessage::new(
                "services.dump.warnings.trigger_order_unavailable",
                format!("trigger firing order unavailable, triggers are written by name: {err}"),
            )
            .with_arg("detail", err.to_string()),
        ),
    }
    out
}

/// Move the triggers after the other programmable objects (a trigger may
/// call a routine) in firing order: by table, timing and event, then action
/// order. Everything else keeps its place.
fn sort_triggers_by_firing_order(
    routines: &mut [DumpObject],
    firing: &HashMap<String, TriggerFiring>,
) {
    if firing.is_empty() {
        return;
    }
    routines.sort_by_key(|obj| {
        let is_trigger = obj.kind == ObjectKind::Trigger;
        let position = if is_trigger {
            firing.get(&obj.label).cloned()
        } else {
            None
        };
        (is_trigger, position)
    });
}

/// DuckDB's identity columns are sequence defaults; the catalog holds the
/// sequence name inside `column_default` (`nextval('orders_id')`). Keyed by
/// `(schema, table, column)`; a failed read degrades to a warning so the
/// dump still writes every row.
async fn load_duckdb_sequences(
    services: &Services,
    profile: &sqlly_core::ConnectionProfile,
    database: &str,
    warnings: &mut Vec<sqlly_core::DisplayMessage>,
) -> HashMap<(String, String, String), String> {
    const SQL: &str = "SELECT schema_name, table_name, column_name, \
        regexp_extract(column_default, 'nextval\\(''([^'']+)''\\)', 1) AS sequence_name \
        FROM duckdb_columns() WHERE database_name = current_database() \
        AND column_default LIKE 'nextval(%'";
    let mut out = HashMap::new();
    match crate::ddl::collect_rows(
        services.queries.as_ref(),
        profile_with_db(profile, database),
        SQL,
    )
    .await
    {
        Ok(rows) => {
            for row in rows {
                let text = |i: usize| match row.get(i) {
                    Some(CellValue::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let sequence = text(3);
                if !sequence.is_empty() {
                    out.insert((text(0), text(1), text(2)), sequence);
                }
            }
        }
        Err(err) => warnings.push(
            sqlly_core::DisplayMessage::new(
                "services.dump.warnings.sequence_defaults_unavailable",
                format!("sequence defaults unavailable: {err}"),
            )
            .with_arg("detail", err.to_string()),
        ),
    }
    out
}

async fn load_columns(
    services: &Services,
    profile: &sqlly_core::ConnectionProfile,
    database: &str,
    tables: &[DumpObject],
    warnings: &mut Vec<sqlly_core::DisplayMessage>,
) -> StudioResult<HashMap<String, Vec<BulkColumnRow>>> {
    let mut out: HashMap<String, Vec<BulkColumnRow>> = HashMap::new();
    match services.metadata.list_all_columns(profile, database).await {
        Ok(rows) => {
            for row in rows {
                let key = table_key(&row.schema, &row.object);
                out.entry(key).or_default().push(row);
            }
        }
        Err(StudioError::NotImplemented(_)) => {
            for table in tables {
                match services
                    .metadata
                    .list_columns(profile, database, &table.schema, &table.name)
                    .await
                {
                    Ok(nodes) => {
                        let cols = nodes
                            .into_iter()
                            .map(|n| BulkColumnRow {
                                schema: table.schema.clone(),
                                object: table.name.clone(),
                                column: n.name,
                                type_name: n.detail,
                                nullable: None,
                                is_identity: false,
                            })
                            .collect();
                        out.insert(table.label.clone(), cols);
                    }
                    Err(err) => warnings.push(
                        sqlly_core::DisplayMessage::new(
                            "services.dump.warnings.columns_unavailable",
                            format!("{} columns: {err}", table.label),
                        )
                        .with_arg("table_name", table.label.clone())
                        .with_arg("detail", err.to_string()),
                    ),
                }
            }
        }
        Err(err) => return Err(err),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{DumpObject, DumpWriter, apply_structure_order, sort_triggers_by_firing_order};
    use crate::dump_render::DumpLayout;
    use sqlly_lang::sqlmodel::DatabaseType;
    use tempfile::tempdir;

    #[test]
    fn file_per_object_creates_layout_dirs() {
        let dir = tempdir().unwrap();
        let mut writer = DumpWriter::new(
            DatabaseType::Postgres,
            DumpLayout::FilePerObject,
            dir.path().to_path_buf(),
        )
        .unwrap();
        writer.open_part("structure", "dbo.customers").unwrap();
        writer.write_str("CREATE TABLE t;\n").unwrap();
        assert!(dir.path().join("structure").exists());
        assert!(dir.path().join("data").exists());
    }

    fn dump_obj(name: &str, kind: sqlly_core::ObjectKind) -> DumpObject {
        DumpObject {
            schema: "dbo".into(),
            name: name.into(),
            kind,
            label: format!("dbo.{name}"),
        }
    }

    /// Alphabetical order would create `a_second` before `z_first`, and its
    /// `FOLLOWS z_first` would then name a trigger that does not exist yet.
    #[test]
    fn mysql_triggers_are_written_in_firing_order_after_routines() {
        use sqlly_core::ObjectKind;
        let mut routines = vec![
            dump_obj("a_second", ObjectKind::Trigger),
            dump_obj("b_proc", ObjectKind::StoredProcedure),
            dump_obj("m_other_table", ObjectKind::Trigger),
            dump_obj("q_third", ObjectKind::Trigger),
            dump_obj("z_first", ObjectKind::Trigger),
        ];
        let at = |table: &str, order: i64| {
            (
                table.to_string(),
                "BEFORE".to_string(),
                "INSERT".to_string(),
                order,
            )
        };
        let firing = std::collections::HashMap::from([
            ("dbo.z_first".to_string(), at("orders", 1)),
            ("dbo.a_second".to_string(), at("orders", 2)),
            ("dbo.q_third".to_string(), at("orders", 3)),
            ("dbo.m_other_table".to_string(), at("customers", 1)),
        ]);
        sort_triggers_by_firing_order(&mut routines, &firing);
        let names: Vec<&str> = routines.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["b_proc", "m_other_table", "z_first", "a_second", "q_third"]
        );
    }

    /// The pipeline's own order puts views and routines alphabetically; an
    /// explicit order (a Deploy Cart's dependency order) replaces it, and
    /// anything the order doesn't name keeps its place after the named ones.
    #[test]
    fn explicit_structure_order_replaces_alphabetical_views_and_routines() {
        use sqlly_core::ObjectKind as K;
        let mut objects = vec![
            dump_obj("customers", K::Table),
            dump_obj("orders", K::Table),
            dump_obj("v_a_summary", K::View),
            dump_obj("v_b_base", K::View),
            dump_obj("fn_total", K::ScalarFunction),
            dump_obj("fn_total", K::ScalarFunction),
            dump_obj("p_unlisted", K::StoredProcedure),
        ];
        let order: Vec<String> = [
            "dbo.customers",
            "dbo.orders",
            "dbo.fn_total",
            "dbo.v_b_base",
            "dbo.v_a_summary",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        apply_structure_order(&mut objects, &order);
        let labels: Vec<&str> = objects.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "dbo.customers",
                "dbo.orders",
                "dbo.fn_total",
                "dbo.fn_total",
                "dbo.v_b_base",
                "dbo.v_a_summary",
                "dbo.p_unlisted",
            ]
        );
    }
}
