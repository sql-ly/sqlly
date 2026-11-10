//! Scheduled database dump persistence and pure scheduling helpers.

include!("dump_schedule_impl.rs");

/// Store-changed signal: every successful save of the schedule store bumps
/// this generation, so views that cache the store (the Preferences pane)
/// reload only when it actually changed instead of reading the file on every
/// render. `tokio::sync::watch` is executor-agnostic, so a gpui task can await
/// it.
fn store_changed_sender() -> &'static tokio::sync::watch::Sender<u64> {
    static SENDER: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> =
        std::sync::OnceLock::new();
    SENDER.get_or_init(|| tokio::sync::watch::channel(0).0)
}

/// The store's current change generation (see [`subscribe_store_changes`]).
pub fn store_generation() -> u64 {
    *store_changed_sender().borrow()
}

/// A receiver that wakes after every successful save of the schedule store.
pub fn subscribe_store_changes() -> tokio::sync::watch::Receiver<u64> {
    store_changed_sender().subscribe()
}

/// Announce that the schedule store was written.
fn note_store_changed() {
    store_changed_sender().send_modify(|generation| *generation += 1);
}

impl DumpSchedules {
    fn path() -> std::path::PathBuf {
        crate::preferences::preferences_path().join("dump-schedules.json")
    }

    /// Load for app start-up: an unreadable file yields no schedules (and a
    /// log line) — [`Self::save`] still refuses to overwrite it, and the
    /// Scheduled tasks pane shows the error via [`Self::load_checked`].
    pub fn load() -> Self {
        Self::load_checked().unwrap_or_else(|error| {
            tracing::warn!("scheduled tasks not loaded: {error}");
            Self::default()
        })
    }

    /// Load, distinguishing "no file yet" (empty) from "file unreadable".
    pub fn load_checked() -> Result<Self, String> {
        Self::load_from(&Self::path())
    }

    fn load_from(path: &std::path::Path) -> Result<Self, String> {
        match crate::fs_safe::load_json_store(path) {
            crate::fs_safe::StoreLoad::Missing => Ok(Self::default()),
            crate::fs_safe::StoreLoad::Loaded(store) => Ok(store),
            crate::fs_safe::StoreLoad::Corrupt { error } => {
                Err(crate::localization::message(error))
            }
        }
    }

    /// Atomically persist. Refuses when the file on disk exists but does not
    /// parse, so a corrupt store is never silently replaced by what little
    /// the app could load.
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::path())
    }

    fn save_to(&self, path: &std::path::Path) -> Result<(), String> {
        if let crate::fs_safe::StoreLoad::Corrupt { error } =
            crate::fs_safe::load_json_store::<Self>(path)
        {
            return Err(crate::localization::format(
                "settings.workspace.scheduled_dumps.error.corrupt_store",
                &[("error", &crate::localization::message(error))],
            ));
        }
        let text = serde_json::to_string_pretty(self).map_err(|err| err.to_string())?;
        crate::fs_safe::write_atomic(path, text.as_bytes()).map_err(|err| {
            crate::localization::format(
                "settings.workspace.scheduled_dumps.error.save_failed",
                &[
                    ("path", &path.display().to_string()),
                    ("error", &err.to_string()),
                ],
            )
        })?;
        note_store_changed();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::DumpContent;

    fn schedule(cadence: Cadence, last_run: Option<DateTime<Utc>>) -> DumpSchedule {
        DumpSchedule {
            id: uuid::Uuid::new_v4(),
            profile_id: uuid::Uuid::new_v4(),
            database: "Sales".into(),
            output_dir: std::path::PathBuf::from("/tmp/dumps"),
            cadence,
            retention: 3,
            options: DumpOptions::default(),
            kind: ScheduleKind::BuiltinDump,
            enabled: true,
            last_run,
            last_result: None,
        }
    }

    fn stamp() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-15T10:30:00Z")
            .expect("parse")
            .with_timezone(&Utc)
    }

    #[test]
    fn next_due_hourly_from_last_run() {
        let last = Utc::now() - Duration::minutes(30);
        let s = schedule(Cadence::Hourly, Some(last));
        let next = next_due(&s, Utc::now());
        assert_eq!(next, last + Duration::hours(1));
    }

    #[test]
    fn next_due_daily_from_last_run() {
        let last = Utc::now() - Duration::hours(2);
        let s = schedule(Cadence::Daily, Some(last));
        let next = next_due(&s, Utc::now());
        assert_eq!(next, last + Duration::days(1));
    }

    #[test]
    fn next_due_weekly_from_last_run() {
        let last = Utc::now() - Duration::days(2);
        let s = schedule(Cadence::Weekly, Some(last));
        let next = next_due(&s, Utc::now());
        assert_eq!(next, last + Duration::weeks(1));
    }

    #[test]
    fn never_run_is_due_now() {
        let now = Utc::now();
        let s = schedule(Cadence::Daily, None);
        assert!(is_due(&s, now));
        assert_eq!(next_due(&s, now), now);
    }

    fn touch(dir: &std::path::Path, name: &str) {
        std::fs::write(dir.join(name), "x").expect("write");
    }

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read")
            .map(|e| e.expect("entry").file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    fn run_name(s: &DumpSchedule, day: u32) -> String {
        let at = DateTime::parse_from_rfc3339(&format!("2026-01-{day:02}T10:30:00Z"))
            .expect("parse")
            .with_timezone(&Utc);
        scheduled_file_name(s, at, ConnectionKind::PostgresWire)
    }

    #[test]
    fn retention_keeps_newest_n_of_this_schedule_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = schedule(Cadence::Daily, None);
        for day in 1..=5 {
            touch(dir.path(), &run_name(&s, day));
        }
        touch(dir.path(), "other.sql");
        let removed = apply_retention(dir.path(), &s, 2);
        assert_eq!(removed.len(), 3);
        let left = names(dir.path());
        assert!(left.contains(&"other.sql".to_string()));
        assert!(left.contains(&run_name(&s, 4)));
        assert!(left.contains(&run_name(&s, 5)), "newest run is kept");
    }

    #[test]
    fn retention_never_touches_lookalike_names_or_other_schedules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut export = schedule(Cadence::Daily, None);
        export.kind = ScheduleKind::QueryExport {
            query_path: std::path::PathBuf::from("reports/daily.sql"),
            query_name: "daily".into(),
            format: QueryExportFormat::Csv,
        };
        // Same database + query name on another schedule (e.g. another
        // connection whose database is also called Sales).
        let mut twin = export.clone();
        twin.id = uuid::Uuid::new_v4();
        let builtin = schedule(Cadence::Daily, None);
        let prefix = name_prefix(&export);
        for day in 1..=3 {
            touch(dir.path(), &run_name(&export, day));
            touch(dir.path(), &run_name(&twin, day));
            touch(dir.path(), &run_name(&builtin, day));
        }
        // Names that merely start with the prefix, and user files that start
        // with the database stem, must survive every sweep.
        let lookalikes = [
            format!("{prefix}totals_20260101-103000.csv"),
            format!("{prefix}20260101-103000.csv.bak"),
            format!("{prefix}2026010-1030000.csv"),
            "Sales_EU_20260101-103000.sql".to_string(),
            "Sales_report.xlsx".to_string(),
        ];
        for name in &lookalikes {
            touch(dir.path(), name);
        }
        std::fs::create_dir(dir.path().join("Sales_archive")).expect("dir");
        // A user directory shaped like a per-object dump name but holding
        // anything else is never removed.
        let lookalike_dir = format!("{}20260101-000000", name_prefix(&builtin));
        std::fs::create_dir(dir.path().join(&lookalike_dir)).expect("dir");
        touch(&dir.path().join(&lookalike_dir), "notes.txt");

        assert_eq!(apply_retention(dir.path(), &export, 1).len(), 2);
        assert_eq!(apply_retention(dir.path(), &builtin, 1).len(), 2);
        let left = names(dir.path());
        for name in &lookalikes {
            assert!(left.contains(name), "{name} must survive");
        }
        assert!(left.contains(&"Sales_archive".to_string()));
        assert!(left.contains(&lookalike_dir));
        assert!(left.contains(&run_name(&export, 3)), "newest export kept");
        for day in 1..=3 {
            assert!(
                left.contains(&run_name(&twin, day)),
                "twin schedule untouched"
            );
        }
    }

    #[test]
    fn retention_removes_old_per_object_dump_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = schedule(Cadence::Daily, None);
        s.options.layout = DumpLayout::FilePerObject;
        for day in 1..=3 {
            let run = dir.path().join(run_name(&s, day));
            std::fs::create_dir_all(run.join("structure")).expect("structure");
            std::fs::create_dir_all(run.join("data")).expect("data");
            touch(&run, "00_header.sql");
            touch(&run.join("structure"), "001_dbo.Orders.sql");
            touch(&run, "manifest.sql");
        }
        let removed = apply_retention(dir.path(), &s, 1);
        assert_eq!(removed.len(), 2);
        assert_eq!(names(dir.path()), vec![run_name(&s, 3)]);
    }

    #[test]
    fn store_refuses_to_overwrite_a_corrupt_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dump-schedules.json");
        assert!(
            DumpSchedules::load_from(&path)
                .expect("missing")
                .schedules
                .is_empty()
        );
        let store = DumpSchedules {
            schedules: vec![schedule(Cadence::Daily, None)],
        };
        store.save_to(&path).expect("save");
        assert_eq!(
            DumpSchedules::load_from(&path)
                .expect("load")
                .schedules
                .len(),
            1
        );
        std::fs::write(&path, "{ not json").expect("corrupt");
        assert!(DumpSchedules::load_from(&path).is_err());
        assert!(DumpSchedules::default().save_to(&path).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "{ not json",
            "the corrupt file is left for the user"
        );
    }

    /// Views caching the store (the Preferences pane) reload on this signal,
    /// so a successful save must move it and wake subscribers.
    #[test]
    fn saves_signal_store_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dump-schedules.json");
        let mut changes = subscribe_store_changes();
        changes.mark_unchanged();
        let before = store_generation();
        DumpSchedules::default().save_to(&path).expect("save");
        assert!(changes.has_changed().expect("sender alive"));
        assert!(store_generation() > before);
    }

    #[test]
    fn legacy_schedule_json_without_kind_migrates_to_builtin_dump() {
        // The shape dump-schedules.json had before schedule kinds existed.
        let legacy = r#"{
            "id": "5a1f6c4e-9d1b-4f0a-8b0e-2c2d6f7a8b9c",
            "profile_id": "0b3d5f7a-1c2e-4a6b-8d9f-0e1c3b5a7d9f",
            "database": "Sales",
            "output_dir": "/tmp/dumps",
            "cadence": "Daily",
            "retention": 5,
            "options": {
                "content": "StructureOnly",
                "layout": "SingleFile",
                "drop_if_exists": true,
                "include_views": true,
                "include_routines": false,
                "include_sequences": true,
                "include_triggers": false,
                "batch_size": 250,
                "schemas": null
            },
            "enabled": true,
            "last_run": null,
            "last_result": "ok"
        }"#;
        let parsed: DumpSchedule = serde_json::from_str(legacy).expect("legacy json parses");
        assert_eq!(parsed.kind, ScheduleKind::BuiltinDump);
        assert_eq!(parsed.database, "Sales");
        assert_eq!(parsed.retention, 5);
        assert!(parsed.options.drop_if_exists);
        assert!(!parsed.options.include_routines);
        assert_eq!(parsed.options.batch_size, 250);
        assert_eq!(parsed.last_result.as_deref(), Some("ok"));
        // Re-serializing keeps every field (lossless migration on next save).
        let text = serde_json::to_string(&parsed).expect("serialize");
        let back: DumpSchedule = serde_json::from_str(&text).expect("round trip");
        assert_eq!(back, parsed);
    }

    #[test]
    fn schedule_kinds_round_trip() {
        let mut native = schedule(Cadence::Hourly, None);
        native.kind = ScheduleKind::NativeBackup {
            pg_format: PgFormat::Tar,
            pg_no_owner: false,
            mysql_single_transaction: false,
            mysql_extra_args: Vec::new(),
            mysql_strip_definer: false,
        };
        let mut export = schedule(Cadence::Weekly, None);
        export.kind = ScheduleKind::QueryExport {
            query_path: std::path::PathBuf::from("reports/daily.sql"),
            query_name: "daily report".into(),
            format: QueryExportFormat::JsonLines,
        };
        for s in [native, export] {
            let text = serde_json::to_string(&s).expect("serialize");
            let back: DumpSchedule = serde_json::from_str(&text).expect("parse");
            assert_eq!(back.kind, s.kind);
            assert!(!text.contains("password"));
        }
    }

    #[test]
    fn scheduled_file_name_per_kind() {
        let now = stamp();
        let mut builtin = schedule(Cadence::Daily, None);
        builtin.id = uuid::Uuid::from_u128(0x3f2a_9c1b_0000_0000_0000_0000_0000_0001);
        assert_eq!(
            scheduled_file_name(&builtin, now, ConnectionKind::PostgresWire),
            "Sales_3f2a9c1b_20260115-103000.sql"
        );
        builtin.options.layout = DumpLayout::FilePerObject;
        assert_eq!(
            scheduled_file_name(&builtin, now, ConnectionKind::PostgresWire),
            "Sales_3f2a9c1b_20260115-103000"
        );
        let mut native = builtin.clone();
        native.kind = ScheduleKind::NativeBackup {
            pg_format: PgFormat::Custom,
            pg_no_owner: true,
            mysql_single_transaction: true,
            mysql_extra_args: Vec::new(),
            mysql_strip_definer: false,
        };
        assert_eq!(
            scheduled_file_name(&native, now, ConnectionKind::PostgresWire),
            "Sales_native_3f2a9c1b_20260115-103000.dump"
        );
        assert_eq!(
            scheduled_file_name(&native, now, ConnectionKind::MySqlWire),
            "Sales_native_3f2a9c1b_20260115-103000.sql"
        );
        let mut export = builtin.clone();
        export.kind = ScheduleKind::QueryExport {
            query_path: std::path::PathBuf::from("reports/daily report.sql"),
            query_name: "daily report".into(),
            format: QueryExportFormat::JsonLines,
        };
        assert_eq!(
            scheduled_file_name(&export, now, ConnectionKind::PostgresWire),
            "Sales_query_daily_report_3f2a9c1b_20260115-103000.jsonl"
        );
    }

    #[test]
    fn native_dump_request_maps_schedule_fields() {
        let mut s = schedule(Cadence::Daily, None);
        s.options.content = DumpContent::StructureOnly;
        s.kind = ScheduleKind::NativeBackup {
            pg_format: PgFormat::Plain,
            pg_no_owner: false,
            mysql_single_transaction: false,
            mysql_extra_args: Vec::new(),
            mysql_strip_definer: false,
        };
        let output = std::path::PathBuf::from("/tmp/dumps/Sales_native_x.sql");
        let req = native_dump_request(&s, output.clone()).expect("native request");
        assert_eq!(req.database, "Sales");
        assert_eq!(req.output, output);
        assert_eq!(req.pg_format, PgFormat::Plain);
        assert!(!req.pg_no_owner);
        assert!(!req.mysql_single_transaction);
        assert_eq!(req.options.content, DumpContent::StructureOnly);
        // The request drives the vendor argv builders verbatim.
        let ep = sqlly_services::native_dump::Endpoint {
            host: "localhost".into(),
            port: 5432,
            user: "alice".into(),
        };
        let args = sqlly_services::pg_dump_args(&req, &ep);
        assert!(args.contains(&"--schema-only".to_string()));
        assert!(!args.contains(&"--no-owner".to_string()));
        // Other kinds produce no native request.
        assert!(native_dump_request(&schedule(Cadence::Daily, None), output).is_none());
    }

    #[test]
    fn schedules_saved_before_mysqldump_options_load_with_none() {
        let mut s = schedule(Cadence::Daily, None);
        s.kind = ScheduleKind::NativeBackup {
            pg_format: PgFormat::Custom,
            pg_no_owner: true,
            mysql_single_transaction: true,
            mysql_extra_args: Vec::new(),
            mysql_strip_definer: false,
        };
        let mut value = serde_json::to_value(&s).expect("serialize");
        let native = value["kind"]["native_backup"]
            .as_object_mut()
            .expect("native kind object");
        native.remove("mysql_extra_args");
        native.remove("mysql_strip_definer");
        let back: DumpSchedule = serde_json::from_value(value).expect("old schedule loads");
        assert_eq!(back.kind, s.kind);
    }

    #[test]
    fn mysql_native_request_carries_extra_options_and_definer_stripping() {
        let mut s = schedule(Cadence::Daily, None);
        s.kind = ScheduleKind::NativeBackup {
            pg_format: PgFormat::Custom,
            pg_no_owner: true,
            mysql_single_transaction: true,
            mysql_extra_args: vec!["--skip-comments".into()],
            mysql_strip_definer: true,
        };
        let req = native_dump_request(&s, std::path::PathBuf::from("/tmp/out.sql")).expect("req");
        assert_eq!(req.mysql_extra_args, vec!["--skip-comments".to_string()]);
        assert!(req.mysql_strip_definer);
    }

    #[test]
    fn mysql_native_request_drives_mysqldump_argv() {
        let mut s = schedule(Cadence::Daily, None);
        s.options.include_triggers = false;
        s.kind = ScheduleKind::NativeBackup {
            pg_format: PgFormat::Custom,
            pg_no_owner: true,
            mysql_single_transaction: true,
            mysql_extra_args: Vec::new(),
            mysql_strip_definer: false,
        };
        let req = native_dump_request(&s, std::path::PathBuf::from("/tmp/out.sql")).expect("req");
        let ep = sqlly_services::native_dump::Endpoint {
            host: "db.internal".into(),
            port: 3306,
            user: "root".into(),
        };
        let args = sqlly_services::mysqldump_args(&req, &ep, std::path::Path::new("/tmp/my.cnf"));
        assert!(args.contains(&"--single-transaction".to_string()));
        assert!(args.contains(&"--skip-triggers".to_string()));
        assert_eq!(args.last().expect("db arg"), "Sales");
    }

    #[test]
    fn schedule_json_round_trip_has_no_secrets() {
        let schedule = DumpSchedule {
            id: uuid::Uuid::new_v4(),
            profile_id: uuid::Uuid::new_v4(),
            database: "db".into(),
            output_dir: std::path::PathBuf::from("/out"),
            cadence: Cadence::Daily,
            retention: 5,
            options: DumpOptions {
                content: DumpContent::StructureOnly,
                ..DumpOptions::default()
            },
            kind: ScheduleKind::BuiltinDump,
            enabled: true,
            last_run: Some(Utc::now()),
            last_result: Some("ok".into()),
        };
        let text = serde_json::to_string(&schedule).expect("serialize");
        assert!(!text.contains("password"));
        assert!(!text.contains("host"));
        let back: DumpSchedule = serde_json::from_str(&text).expect("parse");
        assert_eq!(back.database, "db");
        assert_eq!(back.options.content, DumpContent::StructureOnly);
    }

    #[test]
    fn query_export_format_labels_and_extensions() {
        assert_eq!(QueryExportFormat::Csv.extension(), "csv");
        assert_eq!(QueryExportFormat::JsonLines.extension(), "jsonl");
        assert_eq!(QueryExportFormat::JsonLines.label(), "JSON Lines");
    }
}
