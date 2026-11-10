use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: uuid::Uuid,
    pub name: String,
    pub enabled: bool,
    pub description: String,
    pub category: String,
    pub owner: String,
    pub date_created: chrono::DateTime<chrono::Utc>,
    pub date_modified: chrono::DateTime<chrono::Utc>,
}

pub struct JobBrowser;

impl JobBrowser {
    pub fn capability_query() -> &'static str {
        "SELECT CASE WHEN OBJECT_ID(N'msdb.dbo.sysjobs') IS NULL THEN 0 ELSE 1 END AS sql_agent_available"
    }

    pub fn list_jobs_query() -> &'static str {
        "SELECT j.job_id, j.name, j.enabled, j.description, c.name AS category, \
         SUSER_SNAME(j.owner_sid) AS owner, j.date_created, j.date_modified \
         FROM msdb.dbo.sysjobs j \
         LEFT JOIN msdb.dbo.syscategories c ON j.category_id = c.category_id \
         ORDER BY j.name"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDefinition {
    pub name: String,
    pub enabled: bool,
    pub description: String,
    pub owner_login: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStepDefinition {
    pub step_name: String,
    pub subsystem: String,
    pub command: String,
    pub database_name: Option<String>,
}

pub struct JobWriter;

impl JobWriter {
    pub fn start_job_sql(job_name: &str) -> String {
        format!(
            "EXEC msdb.dbo.sp_start_job @job_name = {}",
            quote_literal(job_name)
        )
    }

    pub fn stop_job_sql(job_name: &str) -> String {
        format!(
            "EXEC msdb.dbo.sp_stop_job @job_name = {}",
            quote_literal(job_name)
        )
    }

    /// `@delete_history = 1` matches SSMS's default: removing a job clears
    /// its history rows too.
    pub fn delete_job_sql(job_name: &str) -> String {
        format!(
            "EXEC msdb.dbo.sp_delete_job @job_name = {}, @delete_history = 1;",
            quote_literal(job_name)
        )
    }

    /// Enable or disable a job (`sp_update_job @enabled`). Script-first like
    /// start/stop: callers put this in an editor tab for review, never run it
    /// from a dialog.
    pub fn set_job_enabled_sql(job_name: &str, enabled: bool) -> String {
        format!(
            "EXEC msdb.dbo.sp_update_job @job_name = {}, @enabled = {};",
            quote_literal(job_name),
            if enabled { 1 } else { 0 }
        )
    }

    pub fn create_job_sql(job: &JobDefinition, steps: &[JobStepDefinition]) -> String {
        let mut sql = format!(
            "EXEC msdb.dbo.sp_add_job @job_name = {}, @enabled = {}, @description = {}",
            quote_literal(&job.name),
            i32::from(job.enabled),
            quote_literal(&job.description)
        );
        if let Some(owner) = &job.owner_login {
            sql.push_str(&format!(", @owner_login_name = {}", quote_literal(owner)));
        }
        for step in steps {
            sql.push_str(";\n");
            sql.push_str(&format!(
                "EXEC msdb.dbo.sp_add_jobstep @job_name = {}, @step_name = {}, @subsystem = {}, @command = {}",
                quote_literal(&job.name),
                quote_literal(&step.step_name),
                quote_literal(&step.subsystem),
                quote_literal(&step.command)
            ));
            if let Some(database) = &step.database_name {
                sql.push_str(&format!(", @database_name = {}", quote_literal(database)));
            }
        }
        sql.push_str(";\n");
        sql.push_str(&format!(
            "EXEC msdb.dbo.sp_add_jobserver @job_name = {};",
            quote_literal(&job.name)
        ));
        sql
    }

    /// Edit job properties without touching its existing steps or schedules.
    pub fn update_job_sql(original_name: &str, job: &JobDefinition) -> String {
        let mut sql = format!(
            "EXEC msdb.dbo.sp_update_job @job_name = {}, @new_name = {}, @enabled = {}, @description = {}",
            quote_literal(original_name),
            quote_literal(&job.name),
            i32::from(job.enabled),
            quote_literal(&job.description)
        );
        if let Some(owner) = &job.owner_login {
            sql.push_str(&format!(", @owner_login_name = {}", quote_literal(owner)));
        }
        sql.push(';');
        sql
    }

    /// Add a step, or update a known step id. The caller deliberately chooses
    /// which operation to script; this never guesses from a step name.
    pub fn upsert_job_step_sql(
        job_name: &str,
        step_id: Option<i32>,
        step: &JobStepDefinition,
    ) -> String {
        let procedure = if step_id.is_some() {
            "sp_update_jobstep"
        } else {
            "sp_add_jobstep"
        };
        let mut sql = format!(
            "EXEC msdb.dbo.{procedure} @job_name = {}",
            quote_literal(job_name)
        );
        if let Some(step_id) = step_id {
            sql.push_str(&format!(", @step_id = {}", step_id.max(1)));
        }
        sql.push_str(&format!(
            ", @step_name = {}, @subsystem = {}, @command = {}",
            quote_literal(&step.step_name),
            quote_literal(&step.subsystem),
            quote_literal(&step.command)
        ));
        if let Some(database) = &step.database_name {
            sql.push_str(&format!(", @database_name = {}", quote_literal(database)));
        }
        sql.push(';');
        sql
    }
}

pub struct JobReader;

impl JobReader {
    pub fn job_details_query(job_id: &str) -> String {
        format!(
            "SELECT j.job_id, j.name, j.enabled, j.description, j.start_step_id \
             FROM msdb.dbo.sysjobs j WHERE j.job_id = {}",
            quote_literal(job_id)
        )
    }

    pub fn job_steps_query(job_id: &str) -> String {
        format!(
            "SELECT s.step_id, s.step_name, s.subsystem, s.command, s.database_name \
             FROM msdb.dbo.sysjobsteps s WHERE s.job_id = {} ORDER BY s.step_id",
            quote_literal(job_id)
        )
    }
}

fn quote_literal(value: &str) -> String {
    format!("N'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_writer_scripts_delete_with_history_cleanup() {
        let sql = JobWriter::delete_job_sql("Nightly O'Brien load");
        assert!(sql.contains("sp_delete_job @job_name = N'Nightly O''Brien load'"));
        assert!(sql.contains("@delete_history = 1;"));
    }

    #[test]
    fn job_reader_escapes_job_id_literals() {
        let sql = JobReader::job_details_query("abc'def");
        assert!(sql.contains("N'abc''def'"));
    }

    #[test]
    fn job_writer_scripts_start_stop_and_create() {
        assert!(JobBrowser::capability_query().contains("sysjobs"));
        assert!(JobWriter::start_job_sql("Nightly Load").contains("sp_start_job"));
        assert!(JobWriter::stop_job_sql("Nightly Load").contains("sp_stop_job"));
        let sql = JobWriter::create_job_sql(
            &JobDefinition {
                name: "Nightly Load".to_string(),
                enabled: true,
                description: "Load O'Brien".to_string(),
                owner_login: Some("sa".to_string()),
            },
            &[JobStepDefinition {
                step_name: "run".to_string(),
                subsystem: "TSQL".to_string(),
                command: "SELECT 1".to_string(),
                database_name: Some("appdb".to_string()),
            }],
        );
        assert!(sql.contains("sp_add_job"));
        assert!(sql.contains("Load O''Brien"));
        assert!(sql.contains("sp_add_jobstep"));
        assert!(sql.contains("sp_add_jobserver"));
    }

    #[test]
    fn scripts_job_and_step_edits_without_executing() {
        let job = JobDefinition {
            name: "Renamed O'Brien".into(),
            enabled: false,
            description: "review me".into(),
            owner_login: None,
        };
        let update = JobWriter::update_job_sql("Old", &job);
        assert!(update.contains("sp_update_job"));
        assert!(update.contains("@new_name = N'Renamed O''Brien'"));
        let step = JobStepDefinition {
            step_name: "Load".into(),
            subsystem: "TSQL".into(),
            command: "SELECT 'x'".into(),
            database_name: Some("app".into()),
        };
        assert!(JobWriter::upsert_job_step_sql("Old", None, &step).contains("sp_add_jobstep"));
        let edit = JobWriter::upsert_job_step_sql("Old", Some(2), &step);
        assert!(edit.contains("sp_update_jobstep"));
        assert!(edit.contains("@step_id = 2"));
    }
}
