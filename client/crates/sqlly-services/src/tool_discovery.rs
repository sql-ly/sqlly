//! Locate the vendor command-line tools SQLLY can optionally drive
//! (pg_dump, pg_restore, psql, mysqldump, mysql) and the interactive CLIs
//! Open in Terminal launches (sqlcmd, redis-cli, clickhouse-client, duckdb,
//! sqlite3, sqlplus/SQLcl). Discovery is filesystem-only; nothing here
//! connects to a database.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeTool {
    PgDump,
    PgRestore,
    Psql,
    MysqlDump,
    Mysql,
    Sqlcmd,
    RedisCli,
    ClickHouseClient,
    DuckDb,
    Sqlite3,
    Sqlplus,
    /// Oracle SQLcl — its binary is named `sql`.
    Sqlcl,
}

impl NativeTool {
    pub fn all() -> [NativeTool; 12] {
        [
            NativeTool::PgDump,
            NativeTool::PgRestore,
            NativeTool::Psql,
            NativeTool::MysqlDump,
            NativeTool::Mysql,
            NativeTool::Sqlcmd,
            NativeTool::RedisCli,
            NativeTool::ClickHouseClient,
            NativeTool::DuckDb,
            NativeTool::Sqlite3,
            NativeTool::Sqlplus,
            NativeTool::Sqlcl,
        ]
    }

    /// Executable file name without extension ("pg_dump", …).
    pub fn binary_name(self) -> &'static str {
        match self {
            NativeTool::PgDump => "pg_dump",
            NativeTool::PgRestore => "pg_restore",
            NativeTool::Psql => "psql",
            NativeTool::MysqlDump => "mysqldump",
            NativeTool::Mysql => "mysql",
            NativeTool::Sqlcmd => "sqlcmd",
            NativeTool::RedisCli => "redis-cli",
            NativeTool::ClickHouseClient => "clickhouse-client",
            NativeTool::DuckDb => "duckdb",
            NativeTool::Sqlite3 => "sqlite3",
            NativeTool::Sqlplus => "sqlplus",
            NativeTool::Sqlcl => "sql",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            NativeTool::Sqlcl => "sql (SQLcl)",
            _ => self.binary_name(),
        }
    }

    pub fn from_snake_case(name: &str) -> Option<Self> {
        match name {
            "pg_dump" => Some(NativeTool::PgDump),
            "pg_restore" => Some(NativeTool::PgRestore),
            "psql" => Some(NativeTool::Psql),
            "mysqldump" => Some(NativeTool::MysqlDump),
            "mysql" => Some(NativeTool::Mysql),
            "sqlcmd" => Some(NativeTool::Sqlcmd),
            "redis-cli" => Some(NativeTool::RedisCli),
            "clickhouse-client" => Some(NativeTool::ClickHouseClient),
            "duckdb" => Some(NativeTool::DuckDb),
            "sqlite3" => Some(NativeTool::Sqlite3),
            "sqlplus" => Some(NativeTool::Sqlplus),
            "sql" => Some(NativeTool::Sqlcl),
            _ => None,
        }
    }

    pub fn snake_case(self) -> &'static str {
        match self {
            NativeTool::PgDump => "pg_dump",
            NativeTool::PgRestore => "pg_restore",
            NativeTool::Psql => "psql",
            NativeTool::MysqlDump => "mysqldump",
            NativeTool::Mysql => "mysql",
            NativeTool::Sqlcmd => "sqlcmd",
            NativeTool::RedisCli => "redis-cli",
            NativeTool::ClickHouseClient => "clickhouse-client",
            NativeTool::DuckDb => "duckdb",
            NativeTool::Sqlite3 => "sqlite3",
            NativeTool::Sqlplus => "sqlplus",
            NativeTool::Sqlcl => "sql",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLocation {
    pub tool: NativeTool,
    pub path: PathBuf,
    /// Parsed from `<tool> --version`, e.g. "16.3" or "8.4.0"; None if the
    /// version line could not be parsed.
    pub version: Option<String>,
    pub source: ToolSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    Override,
    Path,
    WellKnownDir,
    /// Shipped inside the app bundle (the Mac App Store edition bundles
    /// pg_dump, pg_restore, psql, mysqldump and mysql in Contents/Helpers).
    Bundled,
}

/// `Contents/Helpers` of the macOS app bundle this executable runs from,
/// when it exists. Only the Mac App Store edition ships one.
pub fn bundled_tools_dir() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    bundled_tools_dir_for(&exe).filter(|dir| dir.is_dir())
}

/// Pure: `<bundle>/Contents/Helpers` for an executable at
/// `<bundle>/Contents/MacOS/<name>`, else `None`.
pub fn bundled_tools_dir_for(exe: &Path) -> Option<PathBuf> {
    let macos_dir = exe.parent()?;
    let contents = macos_dir.parent()?;
    (macos_dir.file_name()? == "MacOS" && contents.file_name()? == "Contents")
        .then(|| contents.join("Helpers"))
}

/// Pure: put the bundled tools directory ahead of `PATH`, so a shipped tool
/// wins over whatever the sandbox may (or may not) let us see on `PATH`.
pub fn path_with_bundled_first(bundled: Option<&Path>, path_var: Option<&str>) -> Option<String> {
    match (bundled, path_var) {
        (None, path) => path.map(str::to_string),
        (Some(dir), None) => Some(dir.display().to_string()),
        (Some(dir), Some(path)) => std::env::join_paths(
            std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(path)),
        )
        .ok()
        .and_then(|joined| joined.into_string().ok()),
    }
}

fn executable_name(tool: NativeTool) -> String {
    let base = tool.binary_name();
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn probe_in_dir(dir: &Path, tool: NativeTool, exists: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let candidate = dir.join(executable_name(tool));
    exists(&candidate).then_some(candidate)
}

/// Candidate directories beyond PATH, in priority order, for the current OS.
pub fn well_known_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from(
            "/Applications/Postgres.app/Contents/Versions/latest/bin",
        ));
        if let Ok(entries) = std::fs::read_dir("/opt/homebrew/opt") {
            let mut pg_dirs: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("postgresql@"))
                })
                .map(|p| p.join("bin"))
                .collect();
            pg_dirs.sort_by(|a, b| b.cmp(a));
            dirs.extend(pg_dirs);
        }
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/opt/homebrew/opt/mysql-client/bin"));
        dirs.push(PathBuf::from("/opt/homebrew/opt/sqlcmd/bin"));
        dirs.push(PathBuf::from("/usr/local/mysql/bin"));
    } else if cfg!(windows) {
        if let Ok(entries) = std::fs::read_dir(r"C:\Program Files\PostgreSQL") {
            let mut pg_dirs: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path().join("bin"))
                .filter(|p| p.is_dir())
                .collect();
            pg_dirs.sort_by(|a, b| b.cmp(a));
            dirs.extend(pg_dirs);
        }
        if let Ok(entries) = std::fs::read_dir(r"C:\Program Files\MySQL") {
            let entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
            let mut mysql_dirs: Vec<PathBuf> = entries
                .iter()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("MySQL Server "))
                })
                .map(|e| e.path().join("bin"))
                .collect();
            mysql_dirs.sort_by(|a, b| b.cmp(a));
            dirs.extend(mysql_dirs);
            let mut wb_dirs: Vec<PathBuf> = entries
                .iter()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.starts_with("MySQL Workbench "))
                })
                .map(|e| e.path())
                .collect();
            wb_dirs.sort_by(|a, b| b.cmp(a));
            dirs.extend(wb_dirs);
        }
        if let Ok(entries) = std::fs::read_dir(r"C:\Program Files\MariaDB") {
            let mut maria_dirs: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path().join("bin"))
                .filter(|p| p.is_dir())
                .collect();
            maria_dirs.sort_by(|a, b| b.cmp(a));
            dirs.extend(maria_dirs);
        }
        // The Go sqlcmd installs system-wide here; the ODBC driver ships its
        // own sqlcmd under each versioned Client SDK directory.
        dirs.push(PathBuf::from(r"C:\Program Files\SqlCmd"));
        if let Ok(entries) =
            std::fs::read_dir(r"C:\Program Files\Microsoft SQL Server\Client SDK\ODBC")
        {
            let mut binn_dirs: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path().join(r"Tools\Binn"))
                .filter(|p| p.is_dir())
                .collect();
            binn_dirs.sort_by(|a, b| b.cmp(a));
            dirs.extend(binn_dirs);
        }
    } else {
        if let Ok(entries) = std::fs::read_dir("/usr/lib/postgresql") {
            let mut pg_dirs: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path().join("bin"))
                .filter(|p| p.is_dir())
                .collect();
            pg_dirs.sort_by(|a, b| b.cmp(a));
            dirs.extend(pg_dirs);
        }
        dirs.push(PathBuf::from("/usr/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/opt/mssql-tools/bin"));
        dirs.push(PathBuf::from("/opt/mssql-tools18/bin"));
    }
    dirs
}

/// Pure: the directories named by a `PATH` value, empty entries dropped.
fn path_entries(path_var: &str) -> Vec<PathBuf> {
    let entries: Vec<PathBuf> = if cfg!(windows) {
        split_windows_path_var(path_var)
    } else {
        std::env::split_paths(path_var).collect()
    };
    entries
        .into_iter()
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect()
}

/// Pure: split a Windows `PATH` on `;`, honouring `"…"` around an entry. Unlike
/// `std::env::split_paths`, a `"` with no closing partner is dropped rather than
/// opening a quote that runs to the end and swallows every later entry, so one
/// malformed entry no longer hides the rest of `PATH`.
fn split_windows_path_var(path_var: &str) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for (i, c) in path_var.char_indices() {
        match c {
            '"' if in_quote => in_quote = false,
            '"' => in_quote = path_var[i + 1..].contains('"'),
            ';' if !in_quote => entries.push(PathBuf::from(std::mem::take(&mut current))),
            _ => current.push(c),
        }
    }
    entries.push(PathBuf::from(current));
    entries
}

/// Pure: given the override map, the PATH string, the well-known dirs and a
/// `exists(&Path) -> bool` probe, produce the candidate path per tool.
pub fn resolve_paths(
    overrides: &HashMap<NativeTool, PathBuf>,
    path_var: Option<&str>,
    well_known: &[PathBuf],
    exists: &dyn Fn(&Path) -> bool,
) -> HashMap<NativeTool, (PathBuf, ToolSource)> {
    let mut found = HashMap::new();
    for tool in NativeTool::all() {
        if let Some(override_path) = overrides.get(&tool) {
            if exists(override_path) {
                found.insert(tool, (override_path.clone(), ToolSource::Override));
            }
            continue;
        }
        if let Some(path) = path_var {
            for dir in path_entries(path) {
                if let Some(candidate) = probe_in_dir(&dir, tool, exists) {
                    found.insert(tool, (candidate, ToolSource::Path));
                    break;
                }
            }
        }
        if found.contains_key(&tool) {
            continue;
        }
        for dir in well_known {
            if let Some(candidate) = probe_in_dir(dir, tool, exists) {
                found.insert(tool, (candidate, ToolSource::WellKnownDir));
                break;
            }
        }
    }
    found
}

/// Pure: parse version output from `--version`.
pub fn parse_version_line(tool: NativeTool, line: &str) -> Option<String> {
    let line = line.trim();
    match tool {
        NativeTool::PgDump | NativeTool::PgRestore | NativeTool::Psql => {
            // "pg_dump (PostgreSQL) 16.3"
            let token = line.split_whitespace().last()?;
            token
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
                .then(|| token.to_string())
        }
        NativeTool::MysqlDump | NativeTool::Mysql => {
            // "mysqldump  Ver 8.4.0 for macos14 on arm64 ..."
            // "mysql  Ver 15.1 Distrib 10.11.6-MariaDB ..."
            if let Some(distrib_idx) = line.split_whitespace().position(|w| w == "Distrib") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if distrib_idx + 1 < parts.len() {
                    return Some(
                        parts[distrib_idx + 1]
                            .trim_end_matches(|c: char| {
                                !c.is_ascii_alphanumeric() && c != '-' && c != '.'
                            })
                            .to_string(),
                    );
                }
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 && parts[1] == "Ver" {
                Some(parts[2].to_string())
            } else {
                None
            }
        }
        NativeTool::ClickHouseClient => {
            // "ClickHouse client version 24.3.1.2672 (official build)."
            after_keyword(line, "version")
        }
        NativeTool::Sqlplus | NativeTool::Sqlcl => {
            // "SQL*Plus: Release 19.0.0.0.0 - Production" / "SQLcl: Release 24.1 Production"
            after_keyword(line, "Release")
        }
        NativeTool::Sqlcmd | NativeTool::RedisCli | NativeTool::DuckDb | NativeTool::Sqlite3 => {
            // "Version 16.0.4000.1 NT", "redis-cli 7.2.4", "v1.1.3 19864453f7",
            // "3.45.1 2024-01-30 ...": the first token that looks like a
            // dotted version, with a leading v stripped.
            line.split_whitespace().find_map(|token| {
                let token = token.strip_prefix('v').unwrap_or(token);
                (token.chars().next().is_some_and(|c| c.is_ascii_digit()) && token.contains('.'))
                    .then(|| token.to_string())
            })
        }
    }
}

/// The token after the first occurrence of `keyword`, trimmed of trailing
/// punctuation — for "X version Y" / "X: Release Y" lines.
fn after_keyword(line: &str, keyword: &str) -> Option<String> {
    let mut tokens = line.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == keyword {
            return tokens.next().map(|v| {
                v.trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '.')
                    .to_string()
            });
        }
    }
    None
}

/// Filesystem + one `--version` spawn per found tool (5 s timeout each).
pub async fn locate_all(overrides: &HashMap<NativeTool, PathBuf>) -> Vec<ToolLocation> {
    let well_known = well_known_dirs();
    let bundled = bundled_tools_dir();
    let path_var =
        path_with_bundled_first(bundled.as_deref(), std::env::var("PATH").ok().as_deref());
    let resolved = resolve_paths(overrides, path_var.as_deref(), &well_known, &|p| {
        p.is_file()
    });
    let mut locations = Vec::new();
    for (tool, (path, source)) in resolved {
        let source = match &bundled {
            Some(dir) if source == ToolSource::Path && path.starts_with(dir) => ToolSource::Bundled,
            _ => source,
        };
        let version = probe_version(&path).await;
        locations.push(ToolLocation {
            tool,
            path,
            version,
            source,
        });
    }
    locations.sort_by_key(|loc| loc.tool as u8);
    locations
}

async fn probe_version(path: &Path) -> Option<String> {
    probe_version_within(path, Duration::from_secs(5)).await
}

async fn probe_version_within(path: &Path, limit: Duration) -> Option<String> {
    use tokio::process::Command;
    // `kill_on_drop`: when the timeout fires, dropping the `output()` future
    // must kill the child, or a CLI that waits (for a password, a license
    // prompt) outlives the probe forever. stdin is null so nothing can wait
    // on it in the first place.
    let mut command = Command::new(path);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(limit, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let first = line.lines().next()?;
    let named = NativeTool::all().into_iter().find(|tool| {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(tool.binary_name()))
    });
    // go-sqlcmd opens `--version` with a banner and prints `Version: x.y.z`
    // on a later line, so sqlcmd's version is looked for on every line.
    if named == Some(NativeTool::Sqlcmd) {
        return line.lines().find_map(|l| {
            parse_version_line(NativeTool::Sqlcmd, &l.replace("Version:", "Version "))
        });
    }
    named
        .and_then(|tool| parse_version_line(tool, first))
        .or_else(|| parse_version_line(NativeTool::PgDump, first))
}

/// Which `sqlcmd` is installed. The two share a name and most switches but
/// not all: the Go rewrite (go-sqlcmd, versions 0.x/1.x) and Microsoft's
/// ODBC tool (versions 11 and up) differ on `-N` values and on `-F`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlcmdFlavor {
    /// go-sqlcmd `major.minor`.
    Go { major: u32, minor: u32 },
    /// ODBC sqlcmd, by major version (17 = mssql-tools, 18 = mssql-tools18).
    Odbc { major: u32 },
}

impl SqlcmdFlavor {
    /// Whether `-N` takes the `m`/`o`/`s` encryption values (`-Nm`, `-No`).
    /// go-sqlcmd before 1.4 accepted only `true`/`false`/`disable`/`default`;
    /// ODBC sqlcmd before 18 has a bare `-N` switch.
    pub fn takes_encryption_mode(self) -> bool {
        match self {
            Self::Go { major, minor } => (major, minor) >= (1, 4),
            Self::Odbc { major } => major >= 18,
        }
    }

    /// Whether `-F <host>` means "host name in the server certificate".
    /// go-sqlcmd added it in 1.9 (before that `-F` was the output format);
    /// ODBC sqlcmd has it from 18.
    pub fn takes_host_name_in_certificate(self) -> bool {
        match self {
            Self::Go { major, minor } => (major, minor) >= (1, 9),
            Self::Odbc { major } => major >= 18,
        }
    }
}

/// Pure: classify `sqlcmd -?` (or `--version`) output. go-sqlcmd prints
/// `Version: 1.10.0`; ODBC sqlcmd prints `Version 18.2.0001.1 Linux` under a
/// "Microsoft (R) SQL Server Command Line Tool" banner. The numbering never
/// overlaps (go-sqlcmd is 0.x/1.x, ODBC sqlcmd 11 and up), so the first
/// dotted version decides; a "go-sqlcmd" or "Install/Create" mention marks
/// the Go tool even without one.
pub fn classify_sqlcmd_output(output: &str) -> Option<SqlcmdFlavor> {
    let version = output
        .lines()
        .filter(|line| line.to_ascii_lowercase().contains("version"))
        .find_map(|line| {
            parse_version_line(NativeTool::Sqlcmd, &line.replace("Version:", "Version "))
        });
    let go_banner = output.contains("go-sqlcmd") || output.contains("Install/Create");
    let parts = version.as_deref().map(|v| {
        let mut nums = v.split('.').map(|n| n.parse::<u32>().ok());
        (nums.next().flatten(), nums.next().flatten())
    });
    match parts {
        Some((Some(major), _)) if major >= 10 && !go_banner => Some(SqlcmdFlavor::Odbc { major }),
        Some((Some(major), minor)) => Some(SqlcmdFlavor::Go {
            major,
            minor: minor.unwrap_or(0),
        }),
        _ if go_banner => Some(SqlcmdFlavor::Go { major: 0, minor: 0 }),
        _ => None,
    }
}

/// Run `sqlcmd -?` (both flavors answer it; ODBC sqlcmd has no
/// `--version`) and classify the output. `None` when it cannot be run or
/// read within 5 s. Call off the UI thread.
pub async fn probe_sqlcmd_flavor(path: &Path) -> Option<SqlcmdFlavor> {
    use tokio::process::Command;
    let mut command = Command::new(path);
    command
        .arg("-?")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .ok()?
        .ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    classify_sqlcmd_output(&text)
}

/// Find one tool by kind from a cached discovery list.
pub fn find_tool(locations: &[ToolLocation], tool: NativeTool) -> Option<&ToolLocation> {
    locations.iter().find(|loc| loc.tool == tool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn parse_version_pg() {
        assert_eq!(
            parse_version_line(NativeTool::PgDump, "pg_dump (PostgreSQL) 16.3"),
            Some("16.3".into())
        );
    }

    #[test]
    fn parse_version_mysql() {
        assert_eq!(
            parse_version_line(
                NativeTool::MysqlDump,
                "mysqldump  Ver 8.4.0 for macos14 on arm64 (MySQL Community Server - GPL)"
            ),
            Some("8.4.0".into())
        );
    }

    #[test]
    fn parse_version_mariadb() {
        assert_eq!(
            parse_version_line(
                NativeTool::Mysql,
                "mysql  Ver 15.1 Distrib 10.11.6-MariaDB, for osx10.18 (arm64)"
            ),
            Some("10.11.6-MariaDB".into())
        );
    }

    #[test]
    fn sqlcmd_flavors_are_told_apart_by_their_help_banner() {
        // go-sqlcmd `-?` / `--version` (the first line carries no version).
        let go =
            "sqlcmd: Install/Create/Query SQL Server, Azure SQL, and Tools\n\nVersion: 1.10.0\n";
        let flavor = classify_sqlcmd_output(go).expect("go");
        assert_eq!(
            flavor,
            SqlcmdFlavor::Go {
                major: 1,
                minor: 10
            }
        );
        assert!(flavor.takes_encryption_mode());
        assert!(flavor.takes_host_name_in_certificate());
        let old_go =
            classify_sqlcmd_output("sqlcmd: Install/Create/Query\nVersion: 1.3.1").unwrap();
        assert!(!old_go.takes_encryption_mode());
        assert!(!old_go.takes_host_name_in_certificate());
        let mid_go = classify_sqlcmd_output("Version: 1.8.2 (go-sqlcmd)").unwrap();
        assert!(mid_go.takes_encryption_mode());
        assert!(
            !mid_go.takes_host_name_in_certificate(),
            "-F was --format before 1.9"
        );

        // ODBC sqlcmd.
        let odbc18 = "Microsoft (R) SQL Server Command Line Tool\nVersion 18.2.0001.1 Linux\nCopyright (C) 2017 Microsoft Corporation. All rights reserved.";
        let flavor = classify_sqlcmd_output(odbc18).expect("odbc");
        assert_eq!(flavor, SqlcmdFlavor::Odbc { major: 18 });
        assert!(flavor.takes_encryption_mode() && flavor.takes_host_name_in_certificate());
        let odbc17 = classify_sqlcmd_output(
            "Microsoft (R) SQL Server Command Line Tool\nVersion 17.10.0001.1 Linux",
        )
        .unwrap();
        assert_eq!(odbc17, SqlcmdFlavor::Odbc { major: 17 });
        assert!(!odbc17.takes_encryption_mode());
        assert_eq!(
            classify_sqlcmd_output("Version 16.0.4000.1 NT"),
            Some(SqlcmdFlavor::Odbc { major: 16 })
        );
        assert_eq!(classify_sqlcmd_output("usage: something else"), None);
    }

    #[test]
    fn parse_version_garbage_is_none() {
        assert_eq!(parse_version_line(NativeTool::PgDump, "???"), None);
    }

    #[test]
    fn parse_version_terminal_clis() {
        assert_eq!(
            parse_version_line(
                NativeTool::ClickHouseClient,
                "ClickHouse client version 24.3.1.2672 (official build)."
            ),
            Some("24.3.1.2672".into())
        );
        assert_eq!(
            parse_version_line(
                NativeTool::Sqlplus,
                "SQL*Plus: Release 19.0.0.0.0 - Production on Wed Jan 1 10:00:00 2025"
            ),
            Some("19.0.0.0.0".into())
        );
        assert_eq!(
            parse_version_line(NativeTool::Sqlcl, "SQLcl: Release 24.1 Production"),
            Some("24.1".into())
        );
        assert_eq!(
            parse_version_line(NativeTool::RedisCli, "redis-cli 7.2.4"),
            Some("7.2.4".into())
        );
        assert_eq!(
            parse_version_line(NativeTool::DuckDb, "v1.1.3 19864453f7"),
            Some("1.1.3".into())
        );
        assert_eq!(
            parse_version_line(
                NativeTool::Sqlite3,
                "3.45.1 2024-01-30 16:01:20 e876e51a0ed5c5b3126f52e532044363a014bc594cf67687"
            ),
            Some("3.45.1".into())
        );
        assert_eq!(
            parse_version_line(NativeTool::Sqlcmd, "Version 16.0.4000.1 NT"),
            Some("16.0.4000.1".into())
        );
        assert_eq!(parse_version_line(NativeTool::RedisCli, "???"), None);
    }

    /// A CLI that hangs on `--version` must not outlive the probe: the
    /// timeout drops the future, and `kill_on_drop` kills the child.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hung_version_probe_is_killed_at_the_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = dir.path().join("psql");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let version = probe_version_within(&script, Duration::from_millis(500)).await;
        assert_eq!(version, None);
        // On a loaded machine the timeout can fire before the shell even
        // records its pid. Then the child was killed before it ran: prove
        // nothing from the script is still running instead of reading a pid.
        let Ok(pid) = std::fs::read_to_string(&pid_file) else {
            let mut leftover = true;
            for _ in 0..100 {
                let out = std::process::Command::new("pgrep")
                    .args(["-f", &script.display().to_string()])
                    .output()
                    .unwrap();
                if out.stdout.is_empty() && !pid_file.exists() {
                    leftover = false;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(!leftover, "the probed child was left running");
            return;
        };
        let pid = pid.trim();
        let mut alive = true;
        for _ in 0..100 {
            // A killed-but-unreaped child is a zombie: gone for our purposes.
            let out = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", pid])
                .output()
                .unwrap();
            let stat = String::from_utf8_lossy(&out.stdout);
            let stat = stat.trim();
            if stat.is_empty() || stat.starts_with('Z') {
                alive = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive, "the probed child {pid} was left running");
    }

    #[test]
    fn terminal_cli_names_round_trip() {
        for tool in NativeTool::all() {
            assert_eq!(NativeTool::from_snake_case(tool.snake_case()), Some(tool));
            assert!(!tool.binary_name().is_empty());
            assert!(!tool.label().is_empty());
        }
        assert_eq!(NativeTool::Sqlcl.binary_name(), "sql");
    }

    #[test]
    fn resolve_finds_terminal_clis_in_a_fake_bin_dir() {
        let dir = tempfile::tempdir().unwrap();
        for tool in [
            NativeTool::Sqlcmd,
            NativeTool::RedisCli,
            NativeTool::ClickHouseClient,
            NativeTool::DuckDb,
            NativeTool::Sqlite3,
            NativeTool::Sqlplus,
            NativeTool::Sqlcl,
        ] {
            std::fs::write(dir.path().join(executable_name(tool)), b"").unwrap();
        }
        let resolved = resolve_paths(&HashMap::new(), None, &[dir.path().to_path_buf()], &|p| {
            p.exists()
        });
        for tool in [
            NativeTool::Sqlcmd,
            NativeTool::RedisCli,
            NativeTool::ClickHouseClient,
            NativeTool::DuckDb,
            NativeTool::Sqlite3,
            NativeTool::Sqlplus,
            NativeTool::Sqlcl,
        ] {
            assert!(resolved.contains_key(&tool), "{tool:?} not resolved");
        }
    }

    #[test]
    fn resolve_prefers_override_then_path_then_well_known() {
        // Directory probes append the platform executable suffix (`.exe` on
        // Windows), so build the on-disk names through executable_name; an
        // override is used verbatim and needs no suffix.
        let override_path = PathBuf::from("/override/pg_dump");
        let path_bin = PathBuf::from("/path/bin").join(executable_name(NativeTool::PgDump));
        let well_bin = PathBuf::from("/well/bin").join(executable_name(NativeTool::PgDump));
        let mut overrides = HashMap::new();
        overrides.insert(NativeTool::PgDump, override_path.clone());
        let exists_set: HashSet<PathBuf> =
            [override_path.clone(), path_bin.clone(), well_bin].into();
        let exists = |p: &Path| exists_set.contains(p);
        let resolved = resolve_paths(
            &overrides,
            Some("/path/bin"),
            &[PathBuf::from("/well/bin")],
            &exists,
        );
        assert_eq!(
            resolved.get(&NativeTool::PgDump),
            Some(&(override_path, ToolSource::Override))
        );
        overrides.clear();
        let resolved = resolve_paths(
            &overrides,
            Some("/path/bin"),
            &[PathBuf::from("/well/bin")],
            &exists,
        );
        assert_eq!(
            resolved.get(&NativeTool::PgDump),
            Some(&(path_bin, ToolSource::Path))
        );
    }

    #[test]
    fn malformed_path_entries_do_not_stop_the_search() {
        // A quoted, NUL-bearing or empty PATH entry ahead of the real one must
        // be skipped rather than abort discovery.
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join(executable_name(NativeTool::PgDump));
        std::fs::write(&tool, b"").unwrap();
        let sep = if cfg!(windows) { ";" } else { ":" };
        let path_var = [
            "\"C:\\Program Files\\broken",
            "bad\0entry",
            "",
            dir.path().to_str().unwrap(),
        ]
        .join(sep);
        let resolved = resolve_paths(&HashMap::new(), Some(&path_var), &[], &|p| p.exists());
        assert_eq!(
            resolved.get(&NativeTool::PgDump),
            Some(&(tool, ToolSource::Path))
        );
    }

    #[test]
    fn windows_path_split_keeps_entries_after_an_unclosed_quote() {
        // std's Windows split_paths lets the unclosed quote swallow every
        // later `;`, which hid the real tool directory (build 14444).
        assert_eq!(
            split_windows_path_var("\"C:\\Program Files\\broken;C:\\tools;;D:\\bin"),
            vec![
                PathBuf::from("C:\\Program Files\\broken"),
                PathBuf::from("C:\\tools"),
                PathBuf::new(),
                PathBuf::from("D:\\bin"),
            ]
        );
    }

    #[test]
    fn windows_path_split_honours_balanced_quotes() {
        assert_eq!(
            split_windows_path_var("\"C:\\odd;name\";C:\\tools"),
            vec![PathBuf::from("C:\\odd;name"), PathBuf::from("C:\\tools")]
        );
        assert_eq!(
            split_windows_path_var("\"C:\\Program Files\\PostgreSQL\\16\\bin\""),
            vec![PathBuf::from("C:\\Program Files\\PostgreSQL\\16\\bin")]
        );
    }

    #[test]
    fn empty_path_entries_are_not_probed_as_the_working_directory() {
        let probed = std::cell::RefCell::new(Vec::new());
        let sep = if cfg!(windows) { ";" } else { ":" };
        resolve_paths(&HashMap::new(), Some(sep), &[], &|p| {
            probed.borrow_mut().push(p.to_path_buf());
            false
        });
        assert!(
            probed
                .borrow()
                .iter()
                .all(|p| p.parent() != Some(Path::new(""))),
            "probed a bare file name: {:?}",
            probed.borrow()
        );
    }

    #[test]
    fn bundled_tools_dir_is_contents_helpers_of_the_app_bundle() {
        assert_eq!(
            bundled_tools_dir_for(Path::new("/Applications/SQLLY.app/Contents/MacOS/SQLLY")),
            Some(PathBuf::from("/Applications/SQLLY.app/Contents/Helpers"))
        );
        assert_eq!(
            bundled_tools_dir_for(Path::new("/usr/local/bin/SQLLY")),
            None
        );
        assert_eq!(bundled_tools_dir_for(Path::new("SQLLY")), None);
    }

    #[cfg(unix)]
    #[test]
    fn bundled_tools_dir_goes_ahead_of_path() {
        let helpers = Path::new("/A.app/Contents/Helpers");
        assert_eq!(
            path_with_bundled_first(Some(helpers), Some("/usr/bin:/bin")).as_deref(),
            Some("/A.app/Contents/Helpers:/usr/bin:/bin")
        );
        assert_eq!(
            path_with_bundled_first(Some(helpers), None).as_deref(),
            Some("/A.app/Contents/Helpers")
        );
        assert_eq!(
            path_with_bundled_first(None, Some("/usr/bin")).as_deref(),
            Some("/usr/bin")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_bundled_tool_wins_over_one_on_path() {
        let bundle = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        for dir in [bundle.path(), elsewhere.path()] {
            std::fs::write(dir.join("pg_dump"), b"").unwrap();
        }
        let path_var = path_with_bundled_first(
            Some(bundle.path()),
            Some(&elsewhere.path().display().to_string()),
        );
        let resolved = resolve_paths(&HashMap::new(), path_var.as_deref(), &[], &|p| p.is_file());
        assert_eq!(
            resolved[&NativeTool::PgDump].0,
            bundle.path().join("pg_dump")
        );
    }

    #[test]
    fn resolve_missing_override_is_absent() {
        let missing = PathBuf::from("/missing/pg_dump");
        let mut overrides = HashMap::new();
        overrides.insert(NativeTool::PgDump, missing);
        let resolved = resolve_paths(&overrides, None, &[], &|_| false);
        assert!(!resolved.contains_key(&NativeTool::PgDump));
    }

    #[cfg(windows)]
    #[test]
    fn resolve_windows_exe_suffix() {
        let exe = PathBuf::from(r"C:\bin\pg_dump.exe");
        let exists_set: HashSet<PathBuf> = [exe.clone()].into();
        let resolved = resolve_paths(&HashMap::new(), Some(r"C:\bin"), &[], &|p| {
            exists_set.contains(p)
        });
        assert_eq!(
            resolved.get(&NativeTool::PgDump),
            Some(&(exe, ToolSource::Path))
        );
    }

    #[test]
    fn resolve_with_fake_bin_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Name the fake binaries with the platform suffix the resolver probes
        // for (`pg_dump.exe` / `mysqldump.exe` on Windows).
        let pg = dir.path().join(executable_name(NativeTool::PgDump));
        let mysql = dir.path().join(executable_name(NativeTool::MysqlDump));
        std::fs::write(&pg, b"").unwrap();
        std::fs::write(&mysql, b"").unwrap();
        let resolved = resolve_paths(&HashMap::new(), None, &[dir.path().to_path_buf()], &|p| {
            p.exists()
        });
        assert!(resolved.contains_key(&NativeTool::PgDump));
        assert!(resolved.contains_key(&NativeTool::MysqlDump));
    }
}
