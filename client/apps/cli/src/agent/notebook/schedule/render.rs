//! Pure renderers for the OS schedulers. Each takes a job's cron
//! expression and the command to run and returns the file (or crontab
//! line) to install; installing is a thin shell-out in `install.rs`.
//!
//! When a scheduler cannot express the expression exactly (a time zone
//! other than the machine's, or day rules it has no syntax for), the job
//! is installed as a one-minute tick that runs `sqlly notebook schedule
//! tick`, which works the due time out itself with the cron math in
//! `cron.rs`.

use std::collections::BTreeMap;

use super::cron::Cron;

/// Calendar entries above this count fall back to the tick.
const MAX_CALENDAR_ENTRIES: usize = 128;
/// Seconds between ticks.
pub const TICK_SECONDS: u32 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger<T> {
    Calendar(Vec<T>),
    Tick,
}

/// The command a scheduler starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
}

impl Command {
    pub fn run_job(program: &str, job_id: &str) -> Self {
        Self {
            program: program.to_string(),
            args: ["notebook", "schedule", "run-now", job_id, "--scheduled"]
                .map(String::from)
                .to_vec(),
        }
    }

    pub fn tick(program: &str) -> Self {
        Self {
            program: program.to_string(),
            args: ["notebook", "schedule", "tick"].map(String::from).to_vec(),
        }
    }
}

fn full(set: &std::collections::BTreeSet<u32>, min: u32, max: u32) -> bool {
    (min..=max).all(|v| set.contains(&v))
}

/// The day rule as alternatives: each is (days of month, weekdays), `None`
/// meaning any. Alternatives are ORed. `None` overall when a scheduler
/// that ANDs keys in one entry cannot express it.
/// (days of month, weekdays), `None` meaning any.
type DayRule = (Option<Vec<u32>>, Option<Vec<u32>>);

fn day_alternatives(cron: &Cron) -> Option<Vec<DayRule>> {
    let days = (!full(&cron.days, 1, 31)).then(|| cron.days.iter().copied().collect::<Vec<_>>());
    let weekdays =
        (!full(&cron.weekdays, 0, 6)).then(|| cron.weekdays.iter().copied().collect::<Vec<_>>());
    match (days, weekdays) {
        (None, None) => Some(vec![(None, None)]),
        (Some(d), None) => Some(vec![(Some(d), None)]),
        (None, Some(w)) => Some(vec![(None, Some(w))]),
        (Some(d), Some(w)) if cron.days_or() => Some(vec![(Some(d), None), (None, Some(w))]),
        // Both restricted and ANDed: only systemd says that in one entry.
        (Some(_), Some(_)) => None,
    }
}

fn values(set: &std::collections::BTreeSet<u32>, min: u32, max: u32) -> Option<Vec<u32>> {
    (!full(set, min, max)).then(|| set.iter().copied().collect())
}

// ---- launchd ---------------------------------------------------------

/// One `StartCalendarInterval` dictionary (missing keys mean "any").
pub type CalendarEntry = BTreeMap<&'static str, u32>;

/// launchd's calendar uses the machine's zone; an explicit other zone, or
/// an ANDed day rule, ticks instead.
pub fn launchd_trigger(cron: &Cron, foreign_zone: bool) -> Trigger<CalendarEntry> {
    if foreign_zone {
        return Trigger::Tick;
    }
    // Every-minute jobs are what the tick already does.
    if full(&cron.minutes, 0, 59) {
        return Trigger::Tick;
    }
    let Some(days) = day_alternatives(cron) else {
        return Trigger::Tick;
    };
    let one = |v: Option<Vec<u32>>| v.map_or(vec![None], |v| v.into_iter().map(Some).collect());
    let minutes = one(values(&cron.minutes, 0, 59));
    let hours = one(values(&cron.hours, 0, 23));
    let months = one(values(&cron.months, 1, 12));
    let mut out = Vec::new();
    for (day_list, weekday_list) in days {
        for day in one(day_list) {
            for weekday in one(weekday_list.clone()) {
                for month in &months {
                    for hour in &hours {
                        for minute in &minutes {
                            let mut entry = CalendarEntry::new();
                            for (key, v) in [
                                ("Minute", *minute),
                                ("Hour", *hour),
                                ("Day", day),
                                ("Weekday", weekday),
                                ("Month", *month),
                            ] {
                                if let Some(v) = v {
                                    entry.insert(key, v);
                                }
                            }
                            out.push(entry);
                            if out.len() > MAX_CALENDAR_ENTRIES {
                                return Trigger::Tick;
                            }
                        }
                    }
                }
            }
        }
    }
    Trigger::Calendar(out)
}

pub fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// A launchd user agent plist.
pub fn launchd_plist(
    label: &str,
    command: &Command,
    trigger: &Trigger<CalendarEntry>,
    log_path: &str,
) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    out.push_str(&format!(
        "  <key>Label</key>\n  <string>{}</string>\n",
        xml_escape(label)
    ));
    out.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for arg in std::iter::once(&command.program).chain(&command.args) {
        out.push_str(&format!("    <string>{}</string>\n", xml_escape(arg)));
    }
    out.push_str("  </array>\n");
    match trigger {
        Trigger::Tick => {
            out.push_str(&format!(
                "  <key>StartInterval</key>\n  <integer>{TICK_SECONDS}</integer>\n"
            ));
        }
        Trigger::Calendar(entries) => {
            out.push_str("  <key>StartCalendarInterval</key>\n  <array>\n");
            for entry in entries {
                out.push_str("    <dict>\n");
                for key in ["Month", "Day", "Weekday", "Hour", "Minute"] {
                    if let Some(v) = entry.get(key) {
                        out.push_str(&format!(
                            "      <key>{key}</key>\n      <integer>{v}</integer>\n"
                        ));
                    }
                }
                out.push_str("    </dict>\n");
            }
            out.push_str("  </array>\n");
        }
    }
    out.push_str("  <key>ProcessType</key>\n  <string>Background</string>\n");
    out.push_str("  <key>LowPriorityIO</key>\n  <true/>\n");
    let log = xml_escape(log_path);
    out.push_str(&format!(
        "  <key>StandardOutPath</key>\n  <string>{log}</string>\n  <key>StandardErrorPath</key>\n  <string>{log}</string>\n"
    ));
    out.push_str("</dict>\n</plist>\n");
    out
}

// ---- systemd ---------------------------------------------------------

const WEEKDAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn list(values: &Option<Vec<u32>>, width: usize) -> String {
    match values {
        None => "*".to_string(),
        Some(v) => v
            .iter()
            .map(|n| format!("{n:0width$}"))
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// `OnCalendar=` expressions (ORed by systemd). systemd takes the zone in
/// the expression, so it never needs the tick.
pub fn systemd_on_calendar(cron: &Cron, tz: Option<&str>) -> Vec<String> {
    let days = (!full(&cron.days, 1, 31)).then(|| cron.days.iter().copied().collect::<Vec<_>>());
    let weekdays =
        (!full(&cron.weekdays, 0, 6)).then(|| cron.weekdays.iter().copied().collect::<Vec<_>>());
    let alternatives = match (&days, &weekdays) {
        (Some(_), Some(_)) if cron.days_or() => {
            vec![(days.clone(), None), (None, weekdays.clone())]
        }
        _ => vec![(days.clone(), weekdays.clone())],
    };
    let months = values(&cron.months, 1, 12);
    let hours = values(&cron.hours, 0, 23);
    let minutes = values(&cron.minutes, 0, 59);
    alternatives
        .into_iter()
        .map(|(d, w)| {
            let mut text = String::new();
            if let Some(w) = w {
                let names: Vec<&str> = w.iter().map(|i| WEEKDAY_NAMES[*i as usize]).collect();
                text.push_str(&names.join(","));
                text.push(' ');
            }
            text.push_str(&format!(
                "*-{}-{} {}:{}:00",
                list(&months, 2),
                list(&d, 2),
                list(&hours, 2),
                list(&minutes, 2)
            ));
            if let Some(tz) = tz {
                text.push(' ');
                text.push_str(tz);
            }
            text
        })
        .collect()
}

/// Quote one argument for a unit file's `ExecStart=`.
fn systemd_quote(arg: &str) -> String {
    let escaped = arg
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

pub fn systemd_service(description: &str, command: &Command) -> String {
    let exec: Vec<String> = std::iter::once(&command.program)
        .chain(&command.args)
        .map(|a| systemd_quote(a))
        .collect();
    format!(
        "[Unit]\nDescription={}\n\n[Service]\nType=oneshot\nExecStart={}\nNice=10\n",
        description.replace('%', "%%"),
        exec.join(" ")
    )
}

pub fn systemd_timer(description: &str, unit: &str, on_calendar: &[String]) -> String {
    let mut out = format!(
        "[Unit]\nDescription={}\n\n[Timer]\n",
        description.replace('%', "%%")
    );
    for line in on_calendar {
        out.push_str(&format!("OnCalendar={line}\n"));
    }
    out.push_str(&format!(
        "Persistent=true\nUnit={unit}\n\n[Install]\nWantedBy=timers.target\n"
    ));
    out
}

// ---- crontab (Linux without systemd) ----------------------------------

fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// The crontab line for a job, tagged so it can be found and removed.
pub fn crontab_line(cron: &Cron, foreign_zone: bool, program: &str, job_id: &str) -> String {
    let (schedule, command) = if foreign_zone {
        ("* * * * *".to_string(), Command::tick(program))
    } else {
        (cron.expression(), Command::run_job(program, job_id))
    };
    let words: Vec<String> = std::iter::once(&command.program)
        .chain(&command.args)
        .map(|a| shell_quote(a).replace('%', "\\%"))
        .collect();
    format!("{schedule} {} {}", words.join(" "), crontab_marker(job_id))
}

pub fn crontab_marker(job_id: &str) -> String {
    format!("# sqlly-notebook:{job_id}")
}

/// `existing` crontab text without the job's line, plus `line` when given.
pub fn crontab_with(existing: &str, job_id: &str, line: Option<&str>) -> String {
    let marker = crontab_marker(job_id);
    let mut lines: Vec<&str> = existing
        .lines()
        .filter(|l| !l.trim_end().ends_with(&marker))
        .collect();
    if let Some(line) = line {
        lines.push(line);
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

// ---- Windows Task Scheduler --------------------------------------------

/// One Task Scheduler calendar trigger at `hour:minute`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskDays {
    Daily,
    Weekly(Vec<u32>),
    Monthly { days: Vec<u32>, months: Vec<u32> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskTrigger {
    pub hour: u32,
    pub minute: u32,
    pub days: TaskDays,
}

const MAX_TASK_TRIGGERS: usize = 48;

/// Task Scheduler triggers run in the machine's zone; anything else ticks.
pub fn schtasks_trigger(cron: &Cron, foreign_zone: bool) -> Trigger<TaskTrigger> {
    if foreign_zone {
        return Trigger::Tick;
    }
    let Some(days) = day_alternatives(cron) else {
        return Trigger::Tick;
    };
    let months_full = full(&cron.months, 1, 12);
    let mut shapes = Vec::new();
    for (day_list, weekday_list) in days {
        let shape = match (day_list, weekday_list) {
            (None, None) if months_full => TaskDays::Daily,
            (None, None) => TaskDays::Monthly {
                days: (1..=31).collect(),
                months: cron.months.iter().copied().collect(),
            },
            (None, Some(w)) if months_full => TaskDays::Weekly(w),
            (None, Some(_)) => return Trigger::Tick,
            (Some(d), _) => TaskDays::Monthly {
                days: d,
                months: cron.months.iter().copied().collect(),
            },
        };
        shapes.push(shape);
    }
    let mut out = Vec::new();
    for shape in shapes {
        for hour in &cron.hours {
            for minute in &cron.minutes {
                out.push(TaskTrigger {
                    hour: *hour,
                    minute: *minute,
                    days: shape.clone(),
                });
                if out.len() > MAX_TASK_TRIGGERS {
                    return Trigger::Tick;
                }
            }
        }
    }
    Trigger::Calendar(out)
}

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// Arguments joined for `<Arguments>`, quoted the way Windows splits them.
fn windows_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.contains([' ', '\t', '"']) {
                format!("\"{}\"", a.replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A Task Scheduler 1.2 task definition. `start_date` (`YYYY-MM-DD`) only
/// anchors the triggers; it is the job's creation day.
pub fn schtasks_xml(
    description: &str,
    command: &Command,
    trigger: &Trigger<TaskTrigger>,
    start_date: &str,
) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n",
    );
    out.push_str(&format!(
        "  <RegistrationInfo>\n    <Description>{}</Description>\n  </RegistrationInfo>\n  <Triggers>\n",
        xml_escape(description)
    ));
    match trigger {
        Trigger::Tick => {
            out.push_str(&format!(
                "    <TimeTrigger>\n      <Repetition>\n        <Interval>PT{}M</Interval>\n      </Repetition>\n      <StartBoundary>{start_date}T00:00:00</StartBoundary>\n      <Enabled>true</Enabled>\n    </TimeTrigger>\n",
                TICK_SECONDS / 60
            ));
        }
        Trigger::Calendar(triggers) => {
            for t in triggers {
                out.push_str(&format!(
                    "    <CalendarTrigger>\n      <StartBoundary>{start_date}T{:02}:{:02}:00</StartBoundary>\n      <Enabled>true</Enabled>\n",
                    t.hour, t.minute
                ));
                match &t.days {
                    TaskDays::Daily => out.push_str(
                        "      <ScheduleByDay>\n        <DaysInterval>1</DaysInterval>\n      </ScheduleByDay>\n",
                    ),
                    TaskDays::Weekly(days) => {
                        out.push_str("      <ScheduleByWeek>\n        <DaysOfWeek>\n");
                        for d in days {
                            out.push_str(&format!("          <{} />\n", DAY_NAMES[*d as usize]));
                        }
                        out.push_str("        </DaysOfWeek>\n        <WeeksInterval>1</WeeksInterval>\n      </ScheduleByWeek>\n");
                    }
                    TaskDays::Monthly { days, months } => {
                        out.push_str("      <ScheduleByMonth>\n        <DaysOfMonth>\n");
                        for d in days {
                            out.push_str(&format!("          <Day>{d}</Day>\n"));
                        }
                        out.push_str("        </DaysOfMonth>\n        <Months>\n");
                        for m in months {
                            out.push_str(&format!("          <{} />\n", MONTH_NAMES[*m as usize - 1]));
                        }
                        out.push_str("        </Months>\n      </ScheduleByMonth>\n");
                    }
                }
                out.push_str("    </CalendarTrigger>\n");
            }
        }
    }
    out.push_str(
        "  </Triggers>\n  <Principals>\n    <Principal id=\"Author\">\n      <LogonType>InteractiveToken</LogonType>\n      <RunLevel>LeastPrivilege</RunLevel>\n    </Principal>\n  </Principals>\n  <Settings>\n    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n    <StartWhenAvailable>true</StartWhenAvailable>\n    <ExecutionTimeLimit>PT4H</ExecutionTimeLimit>\n    <Enabled>true</Enabled>\n  </Settings>\n",
    );
    out.push_str(&format!(
        "  <Actions Context=\"Author\">\n    <Exec>\n      <Command>{}</Command>\n      <Arguments>{}</Arguments>\n    </Exec>\n  </Actions>\n</Task>\n",
        xml_escape(&command.program),
        xml_escape(&windows_args(&command.args))
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_testkit::golden::GoldenTest;
    use std::path::{Path, PathBuf};

    const EXE: &str = "/Applications/SQLLY.app/Contents/MacOS/sqlly";

    fn golden(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("golden")
            .join("notebook")
            .join(name)
    }

    fn check(actual: &str, name: &str) {
        assert!(
            GoldenTest::compare(actual, golden(name)).unwrap(),
            "{name} changed:\n{actual}"
        );
    }

    fn cron(text: &str) -> Cron {
        Cron::parse(text).unwrap()
    }

    #[test]
    fn launchd_calendar_for_weekday_mornings() {
        let trigger = launchd_trigger(&cron("30 7 * * 1-5"), false);
        let Trigger::Calendar(entries) = &trigger else {
            panic!("expected a calendar");
        };
        assert_eq!(entries.len(), 5);
        let plist = launchd_plist(
            "app.sqlly.notebook.j_demo",
            &Command::run_job(EXE, "j_demo"),
            &trigger,
            "/Users/me/Library/Application Support/SQLLY/Notebooks/logs/j_demo.log",
        );
        check(&plist, "launchd-weekday.plist");
    }

    #[test]
    fn launchd_ticks_for_other_zones_and_anded_days() {
        assert_eq!(launchd_trigger(&cron("0 9 * * *"), true), Trigger::Tick);
        assert_eq!(launchd_trigger(&cron("0 9 */2 * 1"), false), Trigger::Tick);
        assert_eq!(launchd_trigger(&cron("* * * * *"), false), Trigger::Tick);
        // OR'd day rules become separate entries.
        let Trigger::Calendar(entries) = launchd_trigger(&cron("0 0 13 * 5"), false) else {
            panic!();
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].get("Day"), Some(&13));
        assert_eq!(entries[1].get("Weekday"), Some(&5));
        let plist = launchd_plist("x", &Command::tick(EXE), &Trigger::Tick, "/tmp/x.log");
        check(&plist, "launchd-tick.plist");
    }

    #[test]
    fn systemd_units_with_zone_and_or_days() {
        let on = systemd_on_calendar(&cron("0 6 1,15 * mon"), Some("Europe/London"));
        assert_eq!(
            on,
            vec![
                "*-*-01,15 06:00:00 Europe/London".to_string(),
                "Mon *-*-* 06:00:00 Europe/London".to_string()
            ]
        );
        assert_eq!(
            systemd_on_calendar(&cron("*/15 9-17 * * 1-5"), None),
            vec!["Mon,Tue,Wed,Thu,Fri *-*-* 09,10,11,12,13,14,15,16,17:00,15,30,45:00".to_string()]
        );
        let service = systemd_service(
            "SQLLY notebook job j_demo (Revenue 100%)",
            &Command::run_job("/opt/sqlly/bin/sqlly", "j_demo"),
        );
        let timer = systemd_timer(
            "Schedule for SQLLY notebook job j_demo",
            "sqlly-notebook-j_demo.service",
            &on,
        );
        check(
            &format!("{service}\n# ---- timer ----\n{timer}"),
            "systemd-units.txt",
        );
    }

    #[test]
    fn crontab_lines_are_tagged_and_replaceable() {
        let line = crontab_line(&cron("0 7 * * 1-5"), false, "/opt/it's/sqlly", "j_demo");
        assert_eq!(
            line,
            "0 7 * * 1-5 '/opt/it'\\''s/sqlly' 'notebook' 'schedule' 'run-now' 'j_demo' '--scheduled' # sqlly-notebook:j_demo"
        );
        let tick = crontab_line(&cron("0 7 * * *"), true, "/opt/sqlly", "j_demo");
        assert!(tick.starts_with("* * * * * '/opt/sqlly' 'notebook' 'schedule' 'tick'"));
        let existing = "MAILTO=me\n0 1 * * * backup\n5 5 * * * old # sqlly-notebook:j_demo\n";
        let replaced = crontab_with(existing, "j_demo", Some(&line));
        assert_eq!(replaced.lines().count(), 3);
        assert!(replaced.ends_with("# sqlly-notebook:j_demo\n"));
        assert_eq!(
            crontab_with(existing, "j_demo", None),
            "MAILTO=me\n0 1 * * * backup\n"
        );
    }

    #[test]
    fn task_scheduler_weekly_monthly_and_tick() {
        let weekly = schtasks_trigger(&cron("30 7 * * 1-5"), false);
        let xml = schtasks_xml(
            "SQLLY notebook job j_demo: Revenue & Refunds",
            &Command::run_job(r"C:\Program Files\SQLLY\sqlly.exe", "j_demo"),
            &weekly,
            "2026-11-18",
        );
        check(&xml, "schtasks-weekly.xml");
        let Trigger::Calendar(monthly) = schtasks_trigger(&cron("0 6 1 1,7 *"), false) else {
            panic!();
        };
        assert_eq!(
            monthly[0].days,
            TaskDays::Monthly {
                days: vec![1],
                months: vec![1, 7]
            }
        );
        assert_eq!(schtasks_trigger(&cron("0 6 * 1 1"), false), Trigger::Tick);
        assert_eq!(schtasks_trigger(&cron("0 6 * * *"), true), Trigger::Tick);
        let tick = schtasks_xml(
            "t",
            &Command::tick("sqlly.exe"),
            &Trigger::Tick,
            "2026-11-18",
        );
        check(&tick, "schtasks-tick.xml");
    }
}
