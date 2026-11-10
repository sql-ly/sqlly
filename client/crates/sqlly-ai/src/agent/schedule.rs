use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleInfo {
    pub id: i32,
    pub name: String,
    pub enabled: bool,
    pub freq_type: i32,
    pub freq_interval: i32,
    pub active_start_date: i32,
    pub active_end_date: i32,
}

pub struct ScheduleReader;

impl ScheduleReader {
    pub fn list_schedules_query() -> &'static str {
        "SELECT s.schedule_id, s.name, s.enabled, s.freq_type, s.freq_interval, \
         s.active_start_date, s.active_end_date, s.active_start_time \
         FROM msdb.dbo.sysschedules s \
         ORDER BY s.name"
    }
}

pub struct ScheduleWriter;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleDefinition {
    pub name: String,
    pub enabled: bool,
    pub freq_type: i32,
    pub freq_interval: i32,
    pub active_start_time: i32,
}

impl ScheduleWriter {
    pub fn create_schedule_sql(name: &str, freq_type: i32, active_start_time: i32) -> String {
        Self::create_schedule_definition_sql(&ScheduleDefinition {
            name: name.into(),
            enabled: true,
            freq_type,
            freq_interval: 1,
            active_start_time,
        })
    }

    pub fn create_schedule_definition_sql(schedule: &ScheduleDefinition) -> String {
        format!(
            "EXEC msdb.dbo.sp_add_schedule @schedule_name = {}, @enabled = {}, @freq_type = {}, @freq_interval = {}, @active_start_time = {};",
            quote_literal(&schedule.name),
            i32::from(schedule.enabled),
            schedule.freq_type,
            schedule.freq_interval.max(1),
            schedule.active_start_time.clamp(0, 235959)
        )
    }

    pub fn update_schedule_sql(schedule_id: i32, schedule: &ScheduleDefinition) -> String {
        format!(
            "EXEC msdb.dbo.sp_update_schedule @schedule_id = {}, @new_name = {}, @enabled = {}, @freq_type = {}, @freq_interval = {}, @active_start_time = {};",
            schedule_id.max(1),
            quote_literal(&schedule.name),
            i32::from(schedule.enabled),
            schedule.freq_type,
            schedule.freq_interval.max(1),
            schedule.active_start_time.clamp(0, 235959)
        )
    }

    pub fn attach_schedule_sql(job_name: &str, schedule_name: &str) -> String {
        format!(
            "EXEC msdb.dbo.sp_attach_schedule @job_name = {}, @schedule_name = {}",
            quote_literal(job_name),
            quote_literal(schedule_name)
        )
    }

    /// `@force_delete = 0` deliberately: deleting a schedule still attached
    /// to jobs should fail loudly, not silently detach them.
    pub fn delete_schedule_sql(schedule_id: i32) -> String {
        format!(
            "EXEC msdb.dbo.sp_delete_schedule @schedule_id = {}, @force_delete = 0;",
            schedule_id.max(1)
        )
    }
}

fn quote_literal(value: &str) -> String {
    format!("N'{}'", value.replace('\'', "''"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleMessage {
    pub message: sqlly_lang::DisplayMessage,
    /// Resolve these separately and supply their joined text as `weekdays` when
    /// formatting `message`; an empty list means that argument is unused.
    pub weekdays: Vec<sqlly_lang::DisplayMessage>,
}

pub fn describe_schedule_message(
    freq_type: i32,
    freq_interval: i32,
    active_start_time: i32,
) -> ScheduleMessage {
    let time = format_agent_time(active_start_time);
    let key = match (freq_type, freq_interval > 1) {
        (1, _) => "services.agent_schedule.once",
        (4, false) => "services.agent_schedule.daily",
        (4, true) => "services.agent_schedule.every_days",
        (8, _) if weekday_names(freq_interval).is_empty() => "services.agent_schedule.weekly",
        (8, _) => "services.agent_schedule.weekly_days",
        (16, _) => "services.agent_schedule.monthly_day",
        (32, _) => "services.agent_schedule.monthly_relative",
        (64, _) => "services.agent_schedule.agent_starts",
        (128, _) => "services.agent_schedule.computer_idle",
        _ => "services.agent_schedule.unknown",
    };
    let mut message = sqlly_lang::DisplayMessage::new(
        key,
        describe_schedule(freq_type, freq_interval, active_start_time),
    );
    if matches!(freq_type, 1 | 4 | 8 | 16 | 32) {
        message = message.with_arg("time", time);
    }
    if freq_type == 4 && freq_interval > 1 || freq_type == 16 {
        message = message.with_arg("frequency_interval", freq_interval.to_string());
    }
    if freq_type == 8 && !weekday_names(freq_interval).is_empty() {
        message = message.with_arg("weekdays", weekday_names(freq_interval).join(", "));
    }
    if !matches!(freq_type, 1 | 4 | 8 | 16 | 32 | 64 | 128) {
        message = message
            .with_arg("frequency_type", freq_type.to_string())
            .with_arg("frequency_interval", freq_interval.to_string());
    }
    let weekdays = weekday_messages(freq_interval);
    ScheduleMessage { message, weekdays }
}

/// Human recurrence text for a SQL Agent schedule row, computed locally from
/// the `sysschedules` frequency encoding — no server round trip. Unknown
/// combinations render honestly as the raw codes instead of guessing.
pub fn describe_schedule(freq_type: i32, freq_interval: i32, active_start_time: i32) -> String {
    let time = format_agent_time(active_start_time);
    match freq_type {
        1 => format!("Once, at {time}"),
        4 => {
            if freq_interval <= 1 {
                format!("Every day at {time}")
            } else {
                format!("Every {freq_interval} days at {time}")
            }
        }
        8 => {
            let days = weekday_names(freq_interval);
            if days.is_empty() {
                format!("Weekly at {time}")
            } else {
                format!("Weekly on {} at {time}", days.join(", "))
            }
        }
        16 => format!("Monthly on day {freq_interval} at {time}"),
        32 => format!("Monthly (relative) at {time}"),
        64 => "When SQL Server Agent starts".to_string(),
        128 => "When the computer is idle".to_string(),
        other => format!("freq_type {other}, freq_interval {freq_interval}"),
    }
}

/// The next run strictly after `after` for the common schedule shapes (once /
/// daily / weekly / monthly at a fixed time). `None` when the frequency type
/// has no computable next run (agent-start, idle, unknown codes). Dates are
/// naive local date-times, matching how SQL Server Agent stores them.
pub fn next_run_after(
    freq_type: i32,
    freq_interval: i32,
    active_start_time: i32,
    after: chrono::NaiveDateTime,
) -> Option<chrono::NaiveDateTime> {
    use chrono::{Datelike, Duration, NaiveTime};
    let hh = (active_start_time / 10000).clamp(0, 23) as u32;
    let mm = ((active_start_time / 100) % 100).clamp(0, 59) as u32;
    let ss = (active_start_time % 100).clamp(0, 59) as u32;
    let at = NaiveTime::from_hms_opt(hh, mm, ss)?;
    let mut candidate = after.date().and_time(at);
    if candidate <= after {
        candidate += Duration::days(1);
    }
    match freq_type {
        1 | 4 => {
            // Once and plain daily both land on the next occurrence of the
            // start time. (Daily with an interval > 1 needs the anchor date,
            // which sysschedules stores as active_start_date; the plain case
            // covers the overwhelming majority of real schedules.)
            if freq_type == 4 && freq_interval > 1 {
                return None;
            }
            Some(candidate)
        }
        8 => {
            // Weekly bitmask: 1=Sunday … 64=Saturday. Walk forward to the
            // next selected weekday (bounded: at most 7 steps).
            if freq_interval <= 0 {
                return None;
            }
            for _ in 0..7 {
                let bit = 1 << candidate.date().weekday().num_days_from_sunday();
                if freq_interval & bit != 0 {
                    return Some(candidate);
                }
                candidate += Duration::days(1);
            }
            None
        }
        16 => {
            // Monthly on a fixed day-of-month; skip months without that day.
            if !(1..=31).contains(&freq_interval) {
                return None;
            }
            for offset in 0..24 {
                let month0 = after.date().month0() as i32 + offset;
                let year = after.date().year() + month0.div_euclid(12);
                let month = (month0.rem_euclid(12) + 1) as u32;
                if let Some(date) =
                    chrono::NaiveDate::from_ymd_opt(year, month, freq_interval as u32)
                {
                    let when = date.and_time(at);
                    if when > after {
                        return Some(when);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn format_agent_time(active_start_time: i32) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        (active_start_time / 10000).clamp(0, 23),
        ((active_start_time / 100) % 100).clamp(0, 59),
        (active_start_time % 100).clamp(0, 59)
    )
}

fn weekday_names(mask: i32) -> Vec<&'static str> {
    const DAYS: [(i32, &str); 7] = [
        (1, "Sun"),
        (2, "Mon"),
        (4, "Tue"),
        (8, "Wed"),
        (16, "Thu"),
        (32, "Fri"),
        (64, "Sat"),
    ];
    DAYS.iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

fn weekday_messages(mask: i32) -> Vec<sqlly_lang::DisplayMessage> {
    const DAYS: [(i32, &str, &str); 7] = [
        (1, "services.agent_schedule.weekdays.sunday", "Sun"),
        (2, "services.agent_schedule.weekdays.monday", "Mon"),
        (4, "services.agent_schedule.weekdays.tuesday", "Tue"),
        (8, "services.agent_schedule.weekdays.wednesday", "Wed"),
        (16, "services.agent_schedule.weekdays.thursday", "Thu"),
        (32, "services.agent_schedule.weekdays.friday", "Fri"),
        (64, "services.agent_schedule.weekdays.saturday", "Sat"),
    ];
    DAYS.iter()
        .filter(|(bit, _, _)| mask & bit != 0)
        .map(|(_, key, fallback)| sqlly_lang::DisplayMessage::new(key, *fallback))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_writer_scripts_delete_without_force() {
        let sql = ScheduleWriter::delete_schedule_sql(7);
        assert!(sql.contains("sp_delete_schedule @schedule_id = 7"));
        assert!(sql.contains("@force_delete = 0;"));
        // Ids below 1 clamp rather than scripting an invalid call.
        assert!(ScheduleWriter::delete_schedule_sql(0).contains("@schedule_id = 1"));
    }

    #[test]
    fn describes_common_recurrences() {
        assert_eq!(describe_schedule(4, 1, 20000), "Every day at 02:00:00");
        assert_eq!(
            describe_schedule(8, 2 | 8, 93000),
            "Weekly on Mon, Wed at 09:30:00"
        );
        assert_eq!(
            describe_schedule(16, 15, 0),
            "Monthly on day 15 at 00:00:00"
        );
        assert_eq!(describe_schedule(64, 0, 0), "When SQL Server Agent starts");
    }

    #[test]
    fn computes_next_runs_locally() {
        use chrono::NaiveDate;
        let after = NaiveDate::from_ymd_opt(2026, 8, 24)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap(); // a Monday
        // Daily at 02:00 -> tomorrow 02:00 (today's already passed).
        assert_eq!(
            next_run_after(4, 1, 20000, after),
            NaiveDate::from_ymd_opt(2026, 8, 25)
                .unwrap()
                .and_hms_opt(2, 0, 0)
        );
        // Weekly on Wednesday (bit 8) at 09:30.
        assert_eq!(
            next_run_after(8, 8, 93000, after),
            NaiveDate::from_ymd_opt(2026, 8, 26)
                .unwrap()
                .and_hms_opt(9, 30, 0)
        );
        // Monthly on the 15th -> next month (this month's passed).
        assert_eq!(
            next_run_after(16, 15, 120000, after),
            NaiveDate::from_ymd_opt(2026, 9, 15)
                .unwrap()
                .and_hms_opt(12, 0, 0)
        );
        // Agent-start schedules have no computable next run.
        assert_eq!(next_run_after(64, 0, 0, after), None);
    }

    #[test]
    fn scripts_schedule_create_and_attach() {
        assert!(ScheduleReader::list_schedules_query().contains("sysschedules"));
        assert!(
            ScheduleWriter::create_schedule_sql("Every O'Clock", 4, 10000).contains("O''Clock")
        );
        assert!(ScheduleWriter::attach_schedule_sql("job", "sched").contains("sp_attach_schedule"));
        let definition = ScheduleDefinition {
            name: "Weekdays".into(),
            enabled: false,
            freq_type: 8,
            freq_interval: 62,
            active_start_time: 93000,
        };
        let create = ScheduleWriter::create_schedule_definition_sql(&definition);
        assert!(create.contains("sp_add_schedule"));
        assert!(create.contains("@freq_interval = 62"));
        let update = ScheduleWriter::update_schedule_sql(7, &definition);
        assert!(update.contains("sp_update_schedule @schedule_id = 7"));
        assert!(update.contains("@enabled = 0"));
    }
}
