// Shared scheduled-dump types and pure scheduling helpers (included by native and wasm modules).

use chrono::{DateTime, Duration, Utc};
use sqlly_lang::sqlmodel::ConnectionKind;
use sqlly_services::{DumpLayout, DumpOptions};
// Vendor-tool types are native-only (no pg_dump in the browser); the shared
// scheduling math stays cross-platform and the web build just never sees a
// native-backup payload.
#[cfg(not(target_arch = "wasm32"))]
use sqlly_services::{NativeDumpRequest, PgFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Cadence {
    Hourly,
    Daily,
    Weekly,
}

/// What a schedule runs when it fires. `BuiltinDump` is the portable SQL dump;
/// `NativeBackup` drives the engine's vendor tool (pg_dump / mysqldump);
/// `QueryExport` runs one saved query from the SQL library and streams the
/// result set to a CSV or JSON Lines file. Schedules written before kinds
/// existed carry no `kind` key and deserialize as `BuiltinDump`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    #[default]
    BuiltinDump,
    NativeBackup {
        #[cfg(not(target_arch = "wasm32"))]
        pg_format: PgFormat,
        pg_no_owner: bool,
        mysql_single_transaction: bool,
        /// Extra mysqldump options; absent in schedules saved before they
        /// existed.
        #[serde(default)]
        mysql_extra_args: Vec<String>,
        #[serde(default)]
        mysql_strip_definer: bool,
    },
    QueryExport {
        /// Path of the saved query relative to the SQL library root.
        query_path: std::path::PathBuf,
        query_name: String,
        format: QueryExportFormat,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryExportFormat {
    Csv,
    JsonLines,
}

impl QueryExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            QueryExportFormat::Csv => "csv",
            QueryExportFormat::JsonLines => "jsonl",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            QueryExportFormat::Csv => "CSV",
            QueryExportFormat::JsonLines => "JSON Lines",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DumpSchedule {
    pub id: uuid::Uuid,
    pub profile_id: uuid::Uuid,
    pub database: String,
    pub output_dir: std::path::PathBuf,
    pub cadence: Cadence,
    pub retention: usize,
    pub options: DumpOptions,
    #[serde(default)]
    pub kind: ScheduleKind,
    pub enabled: bool,
    pub last_run: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct DumpSchedules {
    pub schedules: Vec<DumpSchedule>,
}

pub fn cadence_duration(cadence: Cadence) -> Duration {
    match cadence {
        Cadence::Hourly => Duration::hours(1),
        Cadence::Daily => Duration::days(1),
        Cadence::Weekly => Duration::weeks(1),
    }
}

pub fn next_due(schedule: &DumpSchedule, now: DateTime<Utc>) -> DateTime<Utc> {
    match schedule.last_run {
        Some(last) => last + cadence_duration(schedule.cadence),
        None => now,
    }
}

pub fn is_due(schedule: &DumpSchedule, now: DateTime<Utc>) -> bool {
    if !schedule.enabled {
        return false;
    }
    next_due(schedule, now) <= now
}

pub fn safe_stem(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
}

/// File extension a native backup produces for this connection kind.
#[cfg(not(target_arch = "wasm32"))]
pub fn native_backup_extension(kind: ConnectionKind, pg_format: PgFormat) -> &'static str {
    match kind {
        ConnectionKind::PostgresWire => match pg_format {
            PgFormat::Custom => "dump",
            PgFormat::Tar => "tar",
            PgFormat::Plain => "sql",
        },
        _ => "sql",
    }
}

/// Timestamp format embedded in every scheduled output name.
pub const STAMP_FORMAT: &str = "%Y%m%d-%H%M%S";

/// The 8-hex-digit tag that makes one schedule's output names its own: two
/// schedules on the same database (or on different connections with the same
/// database name) never share a prefix, so neither prunes the other's files.
pub fn schedule_tag(schedule: &DumpSchedule) -> String {
    schedule.id.simple().to_string()[..8].to_string()
}

/// The output-name prefix every run of one schedule shares:
/// `<db>_<tag>_`, `<db>_native_<tag>_`, or `<db>_query_<query>_<tag>_`. The
/// run's timestamp and extension follow it.
pub fn name_prefix(schedule: &DumpSchedule) -> String {
    let db = safe_stem(&schedule.database);
    let tag = schedule_tag(schedule);
    match &schedule.kind {
        ScheduleKind::BuiltinDump => format!("{db}_{tag}_"),
        ScheduleKind::NativeBackup { .. } => format!("{db}_native_{tag}_"),
        ScheduleKind::QueryExport { query_name, .. } => {
            format!("{db}_query_{}_{tag}_", safe_stem(query_name))
        }
    }
}

/// The `.ext` (with the dot) a run of `schedule` writes, or `""` for a
/// file-per-object built-in dump, which writes a directory.
pub fn scheduled_suffix(
    schedule: &DumpSchedule,
    #[cfg_attr(target_arch = "wasm32", allow(unused_variables))] connection_kind: ConnectionKind,
) -> String {
    match &schedule.kind {
        ScheduleKind::BuiltinDump => {
            if schedule.options.layout == DumpLayout::FilePerObject {
                String::new()
            } else {
                ".sql".to_string()
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        ScheduleKind::NativeBackup { pg_format, .. } => {
            format!(".{}", native_backup_extension(connection_kind, *pg_format))
        }
        // The web build never runs vendor tools; the name only has to stay
        // shape-compatible for retention previews.
        #[cfg(target_arch = "wasm32")]
        ScheduleKind::NativeBackup { .. } => ".sql".to_string(),
        ScheduleKind::QueryExport { format, .. } => format!(".{}", format.extension()),
    }
}

/// The file name one run of `schedule` writes into `schedule.output_dir`.
pub fn scheduled_file_name(
    schedule: &DumpSchedule,
    now: DateTime<Utc>,
    connection_kind: ConnectionKind,
) -> String {
    format!(
        "{}{}{}",
        name_prefix(schedule),
        now.format(STAMP_FORMAT),
        scheduled_suffix(schedule, connection_kind)
    )
}

/// Extensions any run of this schedule kind can have written (the pg_dump
/// format or the export format may have been edited since), so retention
/// keeps recognising older runs of the same schedule.
fn retained_extensions(kind: &ScheduleKind) -> &'static [&'static str] {
    match kind {
        ScheduleKind::BuiltinDump => &["sql"],
        ScheduleKind::NativeBackup { .. } => &["dump", "tar", "sql"],
        ScheduleKind::QueryExport { .. } => &["csv", "jsonl"],
    }
}

/// True when `rest` is exactly `YYYYMMDD-HHMMSS`.
fn is_stamp(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    bytes.len() == 15
        && bytes[8] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 8 || b.is_ascii_digit())
}

/// What a directory entry is to the retention sweep of one schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnedOutput {
    File,
    Directory,
}

/// Whether `name` is exactly an output name this schedule writes:
/// `<prefix><stamp>` plus one of its kind's extensions for a file, or bare
/// (built-in file-per-object dumps only) for a directory.
fn owned_output_shape(name: &str, schedule: &DumpSchedule) -> Option<OwnedOutput> {
    let rest = name.strip_prefix(name_prefix(schedule).as_str())?;
    if rest.len() == 15 {
        return (is_stamp(rest) && schedule.kind == ScheduleKind::BuiltinDump)
            .then_some(OwnedOutput::Directory);
    }
    let (stamp, ext) = rest.split_at_checked(15)?;
    let ext = ext.strip_prefix('.')?;
    (is_stamp(stamp) && retained_extensions(&schedule.kind).contains(&ext))
        .then_some(OwnedOutput::File)
}

/// A directory the built-in file-per-object dump wrote: only its header,
/// epilogue, manifest and `structure/` + `data/` folders of `NNN_*.sql`
/// parts. Anything else inside means someone else's directory — never
/// removed.
fn is_per_object_dump_dir(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut saw_parts_dir = false;
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(file_type) = entry.file_type() else {
            return false;
        };
        match name.as_ref() {
            "00_header.sql" | "99_epilogue.sql" | "manifest.sql" if file_type.is_file() => {}
            "structure" | "data" if file_type.is_dir() => {
                saw_parts_dir = true;
                let Ok(parts) = std::fs::read_dir(entry.path()) else {
                    return false;
                };
                for part in parts {
                    let Ok(part) = part else {
                        return false;
                    };
                    let part_name = part.file_name();
                    let part_name = part_name.to_string_lossy();
                    let numbered = part_name.len() > 8
                        && part_name.as_bytes()[..3].iter().all(u8::is_ascii_digit)
                        && part_name.as_bytes()[3] == b'_'
                        && part_name.ends_with(".sql");
                    if !numbered || !part.file_type().is_ok_and(|t| t.is_file()) {
                        return false;
                    }
                }
            }
            _ => return false,
        }
    }
    saw_parts_dir
}

/// The vendor-tool request one native-backup run spawns. `None` for other kinds.
#[cfg(not(target_arch = "wasm32"))]
pub fn native_dump_request(
    schedule: &DumpSchedule,
    output: std::path::PathBuf,
) -> Option<NativeDumpRequest> {
    match &schedule.kind {
        ScheduleKind::NativeBackup {
            pg_format,
            pg_no_owner,
            mysql_single_transaction,
            mysql_extra_args,
            mysql_strip_definer,
        } => Some(NativeDumpRequest {
            database: schedule.database.clone(),
            output,
            options: schedule.options.clone(),
            tables: None,
            pg_format: *pg_format,
            pg_no_owner: *pg_no_owner,
            mysql_single_transaction: *mysql_single_transaction,
            mysql_extra_args: mysql_extra_args.clone(),
            mysql_strip_definer: *mysql_strip_definer,
        }),
        _ => None,
    }
}

/// Delete all but the newest `keep` outputs of `schedule` in `dir`. Only
/// names exactly shaped like this schedule's runs are candidates (see
/// [`owned_output_shape`]), and a directory is removed only when its
/// contents are a built-in per-object dump, so another schedule's files, a
/// similarly named report, or a user folder are never touched. Returns the
/// removed paths. `keep == 0` keeps everything.
pub fn apply_retention(
    dir: &std::path::Path,
    schedule: &DumpSchedule,
    keep: usize,
) -> Vec<std::path::PathBuf> {
    if keep == 0 {
        return Vec::new();
    }
    let mut entries: Vec<(String, std::path::PathBuf, OwnedOutput)> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(shape) = owned_output_shape(&name, schedule) else {
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            let owned = match shape {
                OwnedOutput::File => file_type.is_file(),
                OwnedOutput::Directory => file_type.is_dir() && is_per_object_dump_dir(&path),
            };
            if owned {
                entries.push((name, path, shape));
            }
        }
    }
    // Same prefix for every candidate, so name order is timestamp order.
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let excess = entries.len().saturating_sub(keep);
    let mut removed = Vec::new();
    for (_, path, shape) in entries.into_iter().take(excess) {
        let ok = match shape {
            OwnedOutput::Directory => std::fs::remove_dir_all(&path).is_ok(),
            OwnedOutput::File => std::fs::remove_file(&path).is_ok(),
        };
        if ok {
            removed.push(path);
        }
    }
    removed
}
