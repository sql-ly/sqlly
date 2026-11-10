//! Build the `sqlly-cli` invocation equivalent to a wizard's configuration.
//!
//! Every "Copy CLI command" button renders through here so the copied line is
//! testable and consistent. Commands use the stable agent surface (`query`,
//! `dump-database`, `restore-file`, `run-file`) and resolve the connection by
//! its saved name. Secrets never appear: the CLI resolves passwords from the
//! OS keyring exactly like the app does.
//!
//! Every line starts with the resolved `sqlly-cli` path (it ships inside the
//! app bundle and is usually not on `PATH`) and uses POSIX shell syntax:
//! single-quoted arguments, `> file` redirects, `| gzip`, and `$(date …)`
//! stamps. That runs as-is in bash/zsh and cron; on Windows it needs Git Bash
//! or WSL (the UI and docs say so). The contract that every generated line
//! parses with the CLI's own argument definitions is pinned by a golden
//! fixture both crates test (see `tests::copied_lines_match_cli_fixture`).

#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

// The native vendor-tool flag set and the table-export targets live behind
// native-only modules; the pure line builders below stay cross-platform.
#[cfg(not(target_arch = "wasm32"))]
use sqlly_lang::sqlmodel::ConnectionKind;
#[cfg(not(target_arch = "wasm32"))]
use sqlly_services::PgFormat;
use sqlly_services::{DumpContent, DumpLayout, DumpOptions};

#[cfg(not(target_arch = "wasm32"))]
use crate::dump_schedule::{DumpSchedule, QueryExportFormat, ScheduleKind};
use crate::results::ExportFormat;
#[cfg(not(target_arch = "wasm32"))]
use crate::table_export::TableExportTarget;

/// The binary name (the browser build has no bundled CLI to locate).
#[cfg(any(target_arch = "wasm32", test))]
const CLI_BINARY: &str = "sqlly-cli";

/// Shell stamp matching the scheduler's `YYYYMMDD-HHMMSS` file-name stamp.
const SHELL_STAMP: &str = "$(date +%Y%m%d-%H%M%S)";

/// The `sqlly-cli` path every copied line starts with: the CLI next to the
/// running app (or on `PATH`), else the bare binary name. Resolved once — it
/// stats a couple of paths and render calls this.
pub fn cli_path() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        PATH.get_or_init(crate::mcp_settings::find_cli_path).clone()
    }
    #[cfg(target_arch = "wasm32")]
    {
        CLI_BINARY.to_string()
    }
}

/// Status text after a copy: says which shell the line is written for.
pub fn copied_status() -> String {
    if cfg!(windows) {
        crate::localization::text("tools.cli_command.copied_windows")
    } else {
        crate::localization::text("tools.cli_command.copied")
    }
}

/// POSIX single-quote escaping; safe values pass through bare for readability.
pub fn quote_arg(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | '@')
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// One positional argument, already rendered for the shell. `leading_hyphen`
/// is about the raw value: clap would read it as an option.
struct Positional {
    text: String,
    leading_hyphen: bool,
}

impl Positional {
    fn raw(value: &str) -> Self {
        Self {
            text: quote_arg(value),
            leading_hyphen: value.starts_with('-'),
        }
    }

    fn rendered(text: String, raw_start: &str) -> Self {
        Self {
            text,
            leading_hyphen: raw_start.starts_with('-'),
        }
    }
}

/// A `sqlly-cli <subcommand>` line under construction.
struct Line {
    program: String,
    subcommand: &'static str,
    positionals: Vec<Positional>,
    options: Vec<String>,
}

impl Line {
    fn new(cli: &str, subcommand: &'static str) -> Self {
        Self {
            program: quote_arg(cli),
            subcommand,
            positionals: Vec::new(),
            options: Vec::new(),
        }
    }

    fn positional(mut self, positional: Positional) -> Self {
        self.positionals.push(positional);
        self
    }

    fn flag(&mut self, flag: &str) {
        self.options.push(flag.to_string());
    }

    /// `--name value`, or `--name=value` when the value starts with `-` (a
    /// leading SQL comment, a negative number) so clap never reads it as an
    /// option.
    fn value(&mut self, name: &str, value: &str) {
        if value.starts_with('-') {
            self.options.push(format!("{name}={}", quote_arg(value)));
        } else {
            self.options.push(format!("{name} {}", quote_arg(value)));
        }
    }

    /// Positionals first for readability; when one starts with `-` the
    /// options go first and `--` ends option parsing before the positionals.
    fn render(self) -> String {
        let mut parts = vec![self.program, self.subcommand.to_string()];
        let hyphen = self.positionals.iter().any(|p| p.leading_hyphen);
        let positionals = self.positionals.into_iter().map(|p| p.text);
        if hyphen {
            parts.extend(self.options);
            parts.push("--".to_string());
            parts.extend(positionals);
        } else {
            parts.extend(positionals);
            parts.extend(self.options);
        }
        parts.join(" ")
    }
}

fn content_value(content: DumpContent) -> Option<&'static str> {
    match content {
        DumpContent::StructureOnly => Some("structure-only"),
        DumpContent::DataOnly => Some("data-only"),
        DumpContent::StructureAndData => None,
    }
}

/// `sqlly-cli dump-database` for the portable built-in SQL dump.
pub fn dump_command(
    cli: &str,
    connection: &str,
    database: &str,
    output: &str,
    options: &DumpOptions,
) -> String {
    dump_line(cli, connection, database, Positional::raw(output), options)
}

/// `dump_command` with a pre-rendered output (the cron variant's `$(date …)`
/// stamp must reach the shell unquoted).
fn dump_line(
    cli: &str,
    connection: &str,
    database: &str,
    output: Positional,
    options: &DumpOptions,
) -> String {
    let mut line = Line::new(cli, "dump-database")
        .positional(Positional::raw(connection))
        .positional(Positional::raw(database))
        .positional(output);
    if let Some(content) = content_value(options.content) {
        line.value("--content", content);
    }
    if options.layout == DumpLayout::FilePerObject {
        line.flag("--per-object");
    }
    if options.drop_if_exists {
        line.flag("--drop-if-exists");
    }
    if options.batch_size != 500 {
        line.value("--batch-size", &options.batch_size.to_string());
    }
    if !options.include_views {
        line.flag("--no-views");
    }
    if !options.include_routines {
        line.flag("--no-routines");
    }
    if !options.include_triggers {
        line.flag("--no-triggers");
    }
    if !options.include_sequences {
        line.flag("--no-sequences");
    }
    if options.include_privileges {
        line.flag("--include-privileges");
    }
    for schema in options.schemas.iter().flatten() {
        line.value("--schema", schema);
    }
    line.render()
}

/// Vendor-tool settings of a native backup, as the dialogs hold them.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub struct NativeSettings {
    pub pg_format: PgFormat,
    pub pg_no_owner: bool,
    pub mysql_single_transaction: bool,
    /// Extra mysqldump options, one argv entry each.
    pub mysql_extra_args: Vec<String>,
    pub mysql_strip_definer: bool,
}

/// `sqlly-cli dump-database --native` for a vendor-tool backup (pg_dump /
/// mysqldump). Only switches the engine's tool honors are emitted: pg_dump
/// takes the schema filter, format and owner setting; mysqldump takes the
/// routine/trigger toggles and single-transaction. Batch size, layout, views
/// and sequences have no vendor equivalent and are omitted rather than faked.
#[cfg(not(target_arch = "wasm32"))]
pub fn native_dump_command(
    cli: &str,
    connection: &str,
    database: &str,
    output: &str,
    options: &DumpOptions,
    kind: ConnectionKind,
    native: NativeSettings,
) -> String {
    native_dump_line(
        cli,
        connection,
        database,
        Positional::raw(output),
        options,
        kind,
        native,
    )
}

/// `native_dump_command` with a pre-rendered output (see `dump_line`).
#[cfg(not(target_arch = "wasm32"))]
fn native_dump_line(
    cli: &str,
    connection: &str,
    database: &str,
    output: Positional,
    options: &DumpOptions,
    kind: ConnectionKind,
    native: NativeSettings,
) -> String {
    let mut line = Line::new(cli, "dump-database")
        .positional(Positional::raw(connection))
        .positional(Positional::raw(database))
        .positional(output);
    line.flag("--native");
    if let Some(content) = content_value(options.content) {
        line.value("--content", content);
    }
    if options.drop_if_exists {
        line.flag("--drop-if-exists");
    }
    match kind {
        ConnectionKind::PostgresWire => {
            for schema in options.schemas.iter().flatten() {
                line.value("--schema", schema);
            }
            match native.pg_format {
                PgFormat::Plain => line.value("--pg-format", "plain"),
                PgFormat::Tar => line.value("--pg-format", "tar"),
                PgFormat::Custom => {}
            }
            if !native.pg_no_owner {
                line.flag("--include-owner");
            }
        }
        ConnectionKind::MySqlWire => {
            if !options.include_routines {
                line.flag("--no-routines");
            }
            if !options.include_triggers {
                line.flag("--no-triggers");
            }
            if !native.mysql_single_transaction {
                line.flag("--no-single-transaction");
            }
            for option in &native.mysql_extra_args {
                line.value("--mysqldump-option", option);
            }
            if native.mysql_strip_definer {
                line.flag("--strip-definer");
            }
        }
        _ => {}
    }
    line.render()
}

/// Settings of a vendor-format restore (pg_restore).
#[derive(Clone, Copy, Debug, Default)]
pub struct RestoreSettings {
    pub clean: bool,
    pub if_exists: bool,
    pub single_transaction: bool,
    pub no_owner: bool,
}

/// `sqlly-cli restore-file` for a vendor-format restore (pg_restore).
pub fn restore_command(
    cli: &str,
    connection: &str,
    database: &str,
    input: &str,
    settings: RestoreSettings,
) -> String {
    let mut line = Line::new(cli, "restore-file")
        .positional(Positional::raw(connection))
        .positional(Positional::raw(database))
        .positional(Positional::raw(input));
    if settings.clean {
        line.flag("--clean");
        if settings.if_exists {
            line.flag("--if-exists");
        }
    }
    if settings.single_transaction {
        line.flag("--single-transaction");
    }
    if settings.no_owner {
        line.flag("--no-owner");
    }
    line.render()
}

/// `sqlly-cli run-file` — the plain-SQL restore/import path.
pub fn run_file_command(cli: &str, connection: &str, database: Option<&str>, file: &str) -> String {
    let mut line = Line::new(cli, "run-file").positional(Positional::raw(connection));
    if let Some(database) = database.filter(|db| !db.trim().is_empty()) {
        line.value("--database", database);
    }
    line.value("--file", file);
    line.render()
}

/// `sqlly-cli query` re-running a SQL string into a formatted export — the
/// CLI equivalent of a result-grid export (whole result; selections and grid
/// filters are client-side and have no CLI equivalent). The CLI runs it
/// read-only, so callers offer it for SELECT sources only.
pub fn query_command(
    cli: &str,
    connection: &str,
    database: Option<&str>,
    sql: &str,
    format: &str,
) -> String {
    let mut line = Line::new(cli, "query").positional(Positional::raw(connection));
    if let Some(database) = database.filter(|db| !db.trim().is_empty()) {
        line.value("--database", database);
    }
    line.value("--sql", sql);
    line.value("--format", format);
    line.value("--limit", "0");
    line.value("--timeout", "0");
    line.render()
}

/// `sqlly-cli query --allow-writes` running an admin script (BACKUP /
/// RESTORE) with no timeout. The CLI still refuses writes on production
/// connections, exactly as the app's safety settings intend.
pub fn admin_sql_command(cli: &str, connection: &str, database: Option<&str>, sql: &str) -> String {
    let mut line = Line::new(cli, "query").positional(Positional::raw(connection));
    if let Some(database) = database.filter(|db| !db.trim().is_empty()) {
        line.value("--database", database);
    }
    line.value("--sql", sql.trim());
    line.flag("--allow-writes");
    line.value("--timeout", "0");
    line.render()
}

/// The `sqlly-cli query --format` value for a grid/table export format, or
/// `None` when the CLI has no renderer for it (XLSX, HTML, XML, SQL, code
/// snippets).
pub fn cli_format_for(format: ExportFormat) -> Option<&'static str> {
    match format {
        ExportFormat::Csv => Some("csv"),
        ExportFormat::Tsv => Some("tsv"),
        ExportFormat::Json => Some("json"),
        ExportFormat::JsonLines => Some("jsonl"),
        ExportFormat::Markdown => Some("md"),
        _ => None,
    }
}

/// Stand-in for the `{date}` pattern token while planning table-export paths;
/// swapped for a shell `$(date +%Y%m%d)` so cron runs get the run's date.
pub const DATE_TOKEN: &str = "SQLLYDATESTAMP";

/// Quote `path` for the shell, letting a planned `DATE_TOKEN` expand at run time.
fn quote_path_with_date(path: &str) -> String {
    let quoted = quote_arg(path);
    if !quoted.contains(DATE_TOKEN) {
        return quoted;
    }
    let expansion = if quoted.starts_with('\'') {
        "'\"$(date +%Y%m%d)\"'"
    } else {
        "\"$(date +%Y%m%d)\""
    };
    quoted.replace(DATE_TOKEN, expansion)
}

/// Export Tables equivalent: one `sqlly-cli query … > file` line per planned
/// table target (piped through `gzip` when the export compresses). `None`
/// when the format has no CLI renderer or nothing is selected. Null/line-break
/// tweaks are app-side options the CLI does not take; the CLI writes its own
/// format defaults (header row, formula-guarded CSV).
#[cfg(not(target_arch = "wasm32"))]
pub fn table_export_commands(
    cli: &str,
    connection: &str,
    database: &str,
    targets: &[TableExportTarget],
    format: ExportFormat,
    gzip: bool,
) -> Option<String> {
    let cli_format = cli_format_for(format)?;
    if targets.is_empty() {
        return None;
    }
    let lines: Vec<String> = targets
        .iter()
        .map(|target| {
            let query = query_command(cli, connection, Some(database), &target.sql, cli_format);
            let output = quote_path_with_date(&target.path.display().to_string());
            if gzip {
                format!("{query} | gzip > {output}")
            } else {
                format!("{query} > {output}")
            }
        })
        .collect();
    Some(lines.join("\n"))
}

/// `sqlly-cli query --file … > output` — the scheduled query-export line.
/// The output is pre-rendered because the cron variant's name carries a
/// `$(date …)` stamp that must reach the shell unquoted.
#[cfg(not(target_arch = "wasm32"))]
fn query_export_line(
    cli: &str,
    connection: &str,
    database: &str,
    query_file: &Path,
    format: QueryExportFormat,
    rendered_output: String,
) -> String {
    let cli_format = match format {
        QueryExportFormat::Csv => "csv",
        QueryExportFormat::JsonLines => "jsonl",
    };
    let mut line = Line::new(cli, "query").positional(Positional::raw(connection));
    if !database.trim().is_empty() {
        line.value("--database", database);
    }
    line.value("--file", &query_file.display().to_string());
    line.value("--format", cli_format);
    line.value("--limit", "0");
    line.value("--timeout", "0");
    format!("{} > {rendered_output}", line.render())
}

/// A `<dir>/<name with $(date …) stamp>` path for cron lines: the directory
/// is quoted, the stamped file name stays bare so the shell expands it.
#[cfg(not(target_arch = "wasm32"))]
fn stamped_output(dir: &Path, stamped_name: &str) -> Positional {
    let dir_text = dir.display().to_string();
    let dir_text = dir_text.trim_end_matches(['/', '\\']);
    Positional::rendered(format!("{}/{stamped_name}", quote_arg(dir_text)), dir_text)
}

/// The one-shot CLI equivalent of what a schedule runs when it fires, with a
/// shell date stamp standing in for the scheduler's timestamped file name
/// (same `<prefix><stamp><ext>` shape, so the schedule's retention also
/// recognises runs made from cron). The schedule itself only fires while the
/// app is open; the CLI line does not prune old files.
#[cfg(not(target_arch = "wasm32"))]
pub fn schedule_command(
    cli: &str,
    schedule: &DumpSchedule,
    connection: &str,
    library_root: &Path,
    connection_kind: ConnectionKind,
) -> String {
    let name = format!(
        "{}{SHELL_STAMP}{}",
        crate::dump_schedule::name_prefix(schedule),
        crate::dump_schedule::scheduled_suffix(schedule, connection_kind)
    );
    let output = stamped_output(&schedule.output_dir, &name);
    match &schedule.kind {
        ScheduleKind::BuiltinDump => dump_line(
            cli,
            connection,
            &schedule.database,
            output,
            &schedule.options,
        ),
        ScheduleKind::NativeBackup {
            pg_format,
            pg_no_owner,
            mysql_single_transaction,
            mysql_extra_args,
            mysql_strip_definer,
        } => native_dump_line(
            cli,
            connection,
            &schedule.database,
            output,
            &schedule.options,
            connection_kind,
            NativeSettings {
                pg_format: *pg_format,
                pg_no_owner: *pg_no_owner,
                mysql_single_transaction: *mysql_single_transaction,
                mysql_extra_args: mysql_extra_args.clone(),
                mysql_strip_definer: *mysql_strip_definer,
            },
        ),
        ScheduleKind::QueryExport {
            query_path, format, ..
        } => query_export_line(
            cli,
            connection,
            &schedule.database,
            &library_root.join(query_path),
            *format,
            output.text,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dump_schedule::Cadence;
    use std::path::PathBuf;

    const CLI: &str = "sqlly-cli";

    #[test]
    fn quote_arg_bares_safe_and_escapes_quotes() {
        assert_eq!(quote_arg("sales"), "sales");
        assert_eq!(quote_arg("/tmp/out.sql"), "/tmp/out.sql");
        assert_eq!(quote_arg("Prod (RO)"), "'Prod (RO)'");
        assert_eq!(quote_arg("it's"), "'it'\\''s'");
        assert_eq!(quote_arg(""), "''");
    }

    #[test]
    fn every_line_starts_with_the_resolved_cli_path() {
        let cli = "/Applications/SQLLY Beta.app/Contents/MacOS/sqlly-cli";
        let cmd = dump_command(cli, "prod", "sales", "/tmp/s.sql", &DumpOptions::default());
        assert_eq!(
            cmd,
            "'/Applications/SQLLY Beta.app/Contents/MacOS/sqlly-cli' dump-database prod sales /tmp/s.sql"
        );
        assert!(cli_path().contains(CLI_BINARY));
    }

    #[test]
    fn dump_command_maps_every_option() {
        let options = DumpOptions {
            content: DumpContent::StructureOnly,
            layout: DumpLayout::FilePerObject,
            drop_if_exists: true,
            include_views: false,
            include_routines: false,
            include_sequences: false,
            include_triggers: false,
            batch_size: 250,
            schemas: Some(vec!["dbo".to_string(), "sales data".to_string()]),
            ..DumpOptions::default()
        };
        let cmd = dump_command(CLI, "Prod (RO)", "sales", "/tmp/s", &options);
        assert_eq!(
            cmd,
            "sqlly-cli dump-database 'Prod (RO)' sales /tmp/s --content structure-only --per-object --drop-if-exists --batch-size 250 --no-views --no-routines --no-triggers --no-sequences --schema dbo --schema 'sales data'"
        );
    }

    #[test]
    fn dump_command_passes_include_privileges() {
        let options = DumpOptions {
            include_privileges: true,
            ..DumpOptions::default()
        };
        let cmd = dump_command(CLI, "pg", "sales", "/tmp/s.sql", &options);
        assert_eq!(
            cmd,
            "sqlly-cli dump-database pg sales /tmp/s.sql --include-privileges"
        );
    }

    fn native(pg_format: PgFormat, pg_no_owner: bool, stxn: bool) -> NativeSettings {
        NativeSettings {
            pg_format,
            pg_no_owner,
            mysql_single_transaction: stxn,
            mysql_extra_args: Vec::new(),
            mysql_strip_definer: false,
        }
    }

    #[test]
    fn native_dump_command_emits_only_the_engines_switches() {
        let options = DumpOptions {
            content: DumpContent::DataOnly,
            layout: DumpLayout::FilePerObject,
            include_views: false,
            include_triggers: false,
            batch_size: 5,
            schemas: Some(vec!["public".into()]),
            ..DumpOptions::default()
        };
        let pg = native_dump_command(
            CLI,
            "pg",
            "sales",
            "/tmp/s.dump",
            &options,
            ConnectionKind::PostgresWire,
            native(PgFormat::Plain, true, true),
        );
        assert_eq!(
            pg,
            "sqlly-cli dump-database pg sales /tmp/s.dump --native --content data-only --schema public --pg-format plain"
        );
        let my = native_dump_command(
            CLI,
            "my",
            "sales",
            "/tmp/s.sql",
            &options,
            ConnectionKind::MySqlWire,
            native(PgFormat::Tar, false, false),
        );
        assert_eq!(
            my,
            "sqlly-cli dump-database my sales /tmp/s.sql --native --content data-only --no-triggers --no-single-transaction"
        );
    }

    #[test]
    fn native_mysql_command_carries_extra_options_and_definer_stripping() {
        let my = native_dump_command(
            CLI,
            "my",
            "sales",
            "/tmp/s.sql",
            &DumpOptions::default(),
            ConnectionKind::MySqlWire,
            NativeSettings {
                mysql_extra_args: vec!["--skip-comments".into(), "--where=id > 5".into()],
                mysql_strip_definer: true,
                ..native(PgFormat::Custom, true, true)
            },
        );
        assert_eq!(
            my,
            "sqlly-cli dump-database my sales /tmp/s.sql --native --mysqldump-option=--skip-comments --mysqldump-option='--where=id > 5' --strip-definer"
        );
        // PostgreSQL never gets mysqldump switches.
        let pg = native_dump_command(
            CLI,
            "pg",
            "sales",
            "/tmp/s.dump",
            &DumpOptions::default(),
            ConnectionKind::PostgresWire,
            NativeSettings {
                mysql_extra_args: vec!["--skip-comments".into()],
                mysql_strip_definer: true,
                ..native(PgFormat::Custom, true, true)
            },
        );
        assert!(!pg.contains("mysqldump-option") && !pg.contains("strip-definer"));
    }

    #[test]
    fn restore_command_flags_and_if_exists_gating() {
        let all = RestoreSettings {
            clean: true,
            if_exists: true,
            single_transaction: true,
            no_owner: true,
        };
        assert_eq!(
            restore_command(CLI, "pg", "sales", "/tmp/in.dump", all),
            "sqlly-cli restore-file pg sales /tmp/in.dump --clean --if-exists --single-transaction --no-owner"
        );
        // --if-exists without --clean is meaningless and is never emitted.
        let if_exists_only = RestoreSettings {
            if_exists: true,
            ..RestoreSettings::default()
        };
        assert_eq!(
            restore_command(CLI, "pg", "sales", "/tmp/in.dump", if_exists_only),
            "sqlly-cli restore-file pg sales /tmp/in.dump"
        );
    }

    #[test]
    fn run_file_and_query_commands() {
        assert_eq!(
            run_file_command(CLI, "prod", Some("sales"), "/tmp/in.sql"),
            "sqlly-cli run-file prod --database sales --file /tmp/in.sql"
        );
        assert_eq!(
            run_file_command(CLI, "prod", None, "/tmp/in.sql"),
            "sqlly-cli run-file prod --file /tmp/in.sql"
        );
        let cmd = query_command(CLI, "Prod (RO)", Some("sales"), "select 1", "csv");
        assert_eq!(
            cmd,
            "sqlly-cli query 'Prod (RO)' --database sales --sql 'select 1' --format csv --limit 0 --timeout 0"
        );
    }

    #[test]
    fn hyphen_values_use_equals_and_hyphen_positionals_follow_double_dash() {
        let cmd = query_command(CLI, "prod", None, "-- daily\nselect 1", "csv");
        assert!(
            cmd.contains(" --sql='-- daily\nselect 1' "),
            "a leading comment must not read as an option: {cmd}"
        );
        let cmd = run_file_command(CLI, "-dev", Some("sales"), "/tmp/in.sql");
        assert_eq!(
            cmd,
            "sqlly-cli run-file --database sales --file /tmp/in.sql -- -dev"
        );
    }

    fn schedule(kind: ScheduleKind) -> DumpSchedule {
        DumpSchedule {
            id: uuid::Uuid::from_u128(0x3f2a_9c1b_0000_0000_0000_0000_0000_0001),
            profile_id: uuid::Uuid::new_v4(),
            database: "sales".into(),
            output_dir: PathBuf::from("/backups"),
            cadence: Cadence::Daily,
            retention: 3,
            options: DumpOptions::default(),
            kind,
            enabled: true,
            last_run: None,
            last_result: None,
        }
    }

    #[test]
    fn schedule_command_builtin_stamps_output_like_the_scheduler() {
        let cmd = schedule_command(
            CLI,
            &schedule(ScheduleKind::BuiltinDump),
            "prod",
            Path::new("/library"),
            ConnectionKind::PostgresWire,
        );
        assert_eq!(
            cmd,
            "sqlly-cli dump-database prod sales /backups/sales_3f2a9c1b_$(date +%Y%m%d-%H%M%S).sql"
        );
    }

    #[test]
    fn schedule_command_native_and_query_export() {
        let native_schedule = schedule(ScheduleKind::NativeBackup {
            pg_format: PgFormat::Custom,
            pg_no_owner: true,
            mysql_single_transaction: true,
            mysql_extra_args: Vec::new(),
            mysql_strip_definer: false,
        });
        let cmd = schedule_command(
            CLI,
            &native_schedule,
            "prod",
            Path::new("/library"),
            ConnectionKind::PostgresWire,
        );
        assert_eq!(
            cmd,
            "sqlly-cli dump-database prod sales /backups/sales_native_3f2a9c1b_$(date +%Y%m%d-%H%M%S).dump --native"
        );
        let export = schedule(ScheduleKind::QueryExport {
            query_path: PathBuf::from("reports/daily.sql"),
            query_name: "daily".into(),
            format: QueryExportFormat::Csv,
        });
        let library = Path::new("/library");
        let cmd = schedule_command(CLI, &export, "prod", library, ConnectionKind::PostgresWire);
        // The query file is a native path (`\` separators on Windows).
        let query_file = quote_arg(&library.join("reports/daily.sql").display().to_string());
        assert_eq!(
            cmd,
            format!(
                "sqlly-cli query prod --database sales --file {query_file} --format csv --limit 0 --timeout 0 > /backups/sales_query_daily_3f2a9c1b_$(date +%Y%m%d-%H%M%S).csv"
            )
        );
    }

    #[test]
    fn cli_format_covers_only_cli_renderers() {
        assert_eq!(cli_format_for(ExportFormat::Tsv), Some("tsv"));
        assert_eq!(cli_format_for(ExportFormat::Markdown), Some("md"));
        assert_eq!(cli_format_for(ExportFormat::Xlsx), None);
        assert_eq!(cli_format_for(ExportFormat::Insert), None);
    }

    fn table_targets() -> Vec<TableExportTarget> {
        vec![
            TableExportTarget {
                schema: "public".into(),
                table: "users".into(),
                sql: "SELECT * FROM \"public\".\"users\"".into(),
                path: PathBuf::from(format!("/out/public.users_{DATE_TOKEN}.csv.gz")),
            },
            TableExportTarget {
                schema: "public".into(),
                table: "order lines".into(),
                sql: "SELECT * FROM \"public\".\"order lines\"".into(),
                path: PathBuf::from(format!("/out/public.order lines_{DATE_TOKEN}.csv.gz")),
            },
        ]
    }

    #[test]
    fn table_export_commands_one_line_per_table_with_date_and_gzip() {
        let targets = table_targets();
        let text = table_export_commands(CLI, "prod", "shop", &targets, ExportFormat::Csv, true)
            .expect("csv has a CLI renderer");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "sqlly-cli query prod --database shop --sql 'SELECT * FROM \"public\".\"users\"' --format csv --limit 0 --timeout 0 | gzip > /out/public.users_\"$(date +%Y%m%d)\".csv.gz"
        );
        assert!(
            lines[1].ends_with("| gzip > '/out/public.order lines_'\"$(date +%Y%m%d)\"'.csv.gz'")
        );
        assert!(
            table_export_commands(CLI, "prod", "shop", &targets, ExportFormat::Xlsx, false)
                .is_none()
        );
        assert!(
            table_export_commands(CLI, "prod", "shop", &[], ExportFormat::Csv, false).is_none()
        );
    }

    #[test]
    fn admin_sql_command_allows_writes_without_timeout() {
        let cmd = admin_sql_command(
            CLI,
            "mssql",
            Some("master"),
            "BACKUP DATABASE [Sales] TO DISK = N'/b/Sales.bak'\n",
        );
        assert!(cmd.starts_with("sqlly-cli query mssql --database master --sql 'BACKUP DATABASE [Sales] TO DISK = N'\\''/b/Sales.bak'\\'''"));
        assert!(cmd.ends_with(" --allow-writes --timeout 0"));
        assert!(!cmd.contains('\n'));
    }

    /// Every generator, with the awkward inputs (spaces, quotes, a SQL
    /// comment first, a hyphen-leading connection name, date stamps, gzip),
    /// rendered with a space-containing CLI path. The same fixture is parsed
    /// by the CLI crate's own clap definition in `sqlly-cli`'s
    /// `copied_cli_lines_parse` test, so a flag rename on either side fails
    /// one of the two tests.
    fn fixture_lines() -> Vec<(&'static str, String)> {
        let cli = "/opt/SQLLY Tools/sqlly-cli";
        let every_option = DumpOptions {
            content: DumpContent::StructureOnly,
            layout: DumpLayout::FilePerObject,
            drop_if_exists: true,
            include_views: false,
            include_routines: false,
            include_sequences: false,
            include_triggers: false,
            batch_size: 250,
            schemas: Some(vec!["dbo".to_string(), "sales data".to_string()]),
            ..DumpOptions::default()
        };
        let mut per_object = schedule(ScheduleKind::BuiltinDump);
        per_object.output_dir = PathBuf::from("/var/backups/it's here");
        per_object.options.layout = DumpLayout::FilePerObject;
        let my_native = schedule(ScheduleKind::NativeBackup {
            pg_format: PgFormat::Custom,
            pg_no_owner: true,
            mysql_single_transaction: false,
            mysql_extra_args: vec!["--skip-comments".into()],
            mysql_strip_definer: true,
        });
        let export = schedule(ScheduleKind::QueryExport {
            query_path: PathBuf::from("daily report.sql"),
            query_name: "daily report".into(),
            format: QueryExportFormat::JsonLines,
        });
        vec![
            (
                "dump-every-option",
                dump_command(cli, "Prod (RO)", "sales", "/tmp/s", &every_option),
            ),
            (
                "dump-native-postgres",
                native_dump_command(
                    cli,
                    "pg",
                    "sales",
                    "/tmp/s.tar",
                    &every_option,
                    ConnectionKind::PostgresWire,
                    native(PgFormat::Tar, false, true),
                ),
            ),
            (
                "dump-native-mysql",
                native_dump_command(
                    cli,
                    "my",
                    "shop",
                    "/tmp/shop.sql",
                    &every_option,
                    ConnectionKind::MySqlWire,
                    NativeSettings {
                        mysql_extra_args: vec![
                            "--where=id > 5".into(),
                            "--column-statistics=0".into(),
                        ],
                        mysql_strip_definer: true,
                        ..native(PgFormat::Custom, true, false)
                    },
                ),
            ),
            (
                "restore-pg-archive",
                restore_command(
                    cli,
                    "pg",
                    "sales",
                    "/tmp/in put.dump",
                    RestoreSettings {
                        clean: true,
                        if_exists: true,
                        single_transaction: true,
                        no_owner: true,
                    },
                ),
            ),
            (
                "run-file-hyphen-connection",
                run_file_command(cli, "-dev", Some("sales"), "/tmp/in.sql"),
            ),
            (
                "query-leading-comment",
                query_command(
                    cli,
                    "prod",
                    Some("sales"),
                    "-- daily totals\nSELECT -1 AS n, 'it''s' AS s",
                    "csv",
                ),
            ),
            (
                "query-negative-literal",
                query_command(cli, "prod", None, "-1 + 2", "json"),
            ),
            (
                "admin-backup-script",
                admin_sql_command(
                    cli,
                    "mssql",
                    Some("master"),
                    "BACKUP DATABASE [Sales]\nTO DISK = N'/b/Sales.bak'",
                ),
            ),
            (
                "table-export-gzip-dated",
                table_export_commands(
                    cli,
                    "prod",
                    "shop",
                    &table_targets(),
                    ExportFormat::Csv,
                    true,
                )
                .expect("csv renders"),
            ),
            (
                "schedule-per-object-dump",
                schedule_command(cli, &per_object, "prod", Path::new(""), ConnectionKind::Tds),
            ),
            (
                "schedule-native-mysql",
                schedule_command(
                    cli,
                    &my_native,
                    "my",
                    Path::new(""),
                    ConnectionKind::MySqlWire,
                ),
            ),
            (
                "schedule-query-export",
                schedule_command(
                    cli,
                    &export,
                    "prod",
                    Path::new(""),
                    ConnectionKind::PostgresWire,
                ),
            ),
        ]
    }

    #[test]
    fn copied_lines_match_cli_fixture() {
        let mut text = String::from(
            "# Generated by sqlly-gpui cli_command tests (SQLLY_UPDATE_GOLDEN=1 to refresh).\n\
             # Parsed by sqlly-cli's clap definition in its copied_cli_lines_parse test.\n",
        );
        for (name, line) in fixture_lines() {
            text.push_str(&format!("### {name}\n{line}\n"));
        }
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../cli/tests/fixtures/copied-cli/lines.txt");
        assert!(
            sqlly_testkit::golden::GoldenTest::compare(&text, &fixture).expect("fixture"),
            "copied CLI lines changed; refresh with SQLLY_UPDATE_GOLDEN=1 cargo test -p sqlly-gpui --lib copied_lines_match_cli_fixture, then run cargo test -p sqlly-cli --bin sqlly-cli copied_cli_lines_parse"
        );
    }
}
