use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobHistoryEntry {
    pub instance_id: i32,
    pub job_id: uuid::Uuid,
    pub job_name: String,
    pub step_id: i32,
    pub step_name: String,
    pub run_status: RunStatus,
    pub run_date: chrono::NaiveDateTime,
    pub run_duration: i32,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Failed = 0,
    Succeeded = 1,
    Retry = 2,
    Canceled = 3,
    InProgress = 4,
    Unknown = 5,
}

pub struct JobHistoryReader;

impl JobHistoryReader {
    pub fn history_query(job_id: Option<&str>) -> String {
        match job_id {
            Some(id) => format!(
                "SELECT h.instance_id, h.job_id, j.name AS job_name, h.step_id, h.step_name, \
                 h.run_status, h.run_date, h.run_duration, h.message \
                 FROM msdb.dbo.sysjobhistory h \
                 INNER JOIN msdb.dbo.sysjobs j ON h.job_id = j.job_id \
                 WHERE h.job_id = {} \
                 ORDER BY h.instance_id DESC",
                quote_literal(id)
            ),
            None => "SELECT h.instance_id, h.job_id, j.name AS job_name, h.step_id, h.step_name, \
                 h.run_status, h.run_date, h.run_duration, h.message \
                 FROM msdb.dbo.sysjobhistory h \
                 INNER JOIN msdb.dbo.sysjobs j ON h.job_id = j.job_id \
                 ORDER BY h.instance_id DESC"
                .to_string(),
        }
    }
}

fn quote_literal(value: &str) -> String {
    format!("N'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_query_escapes_job_id_literal() {
        let sql = JobHistoryReader::history_query(Some("abc'def"));
        assert!(sql.contains("WHERE h.job_id = N'abc''def'"));
    }
}
