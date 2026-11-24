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
    /// What the job does when the step succeeds. `None` leaves it alone: an
    /// edited step keeps its current setting, a new one gets SQL Server's
    /// default (quit reporting success).
    #[serde(default)]
    pub on_success: Option<StepFlow>,
    /// What the job does when the step fails; `None` as for `on_success`
    /// (a new step quits reporting failure).
    #[serde(default)]
    pub on_failure: Option<StepFlow>,
}

/// Where SQL Server Agent goes after a step finishes: `sysjobsteps`'
/// `on_success_action` / `on_fail_action` codes 1-4, with the target step of
/// code 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepFlow {
    QuitWithSuccess,
    QuitWithFailure,
    NextStep,
    GoToStep(i32),
}

impl StepFlow {
    pub fn action_code(self) -> i32 {
        match self {
            StepFlow::QuitWithSuccess => 1,
            StepFlow::QuitWithFailure => 2,
            StepFlow::NextStep => 3,
            StepFlow::GoToStep(_) => 4,
        }
    }

    /// Read back a `sysjobsteps` action code and its step id.
    pub fn from_catalog(action: i32, step_id: i32) -> Option<StepFlow> {
        match action {
            1 => Some(StepFlow::QuitWithSuccess),
            2 => Some(StepFlow::QuitWithFailure),
            3 => Some(StepFlow::NextStep),
            4 if step_id > 0 => Some(StepFlow::GoToStep(step_id)),
            _ => None,
        }
    }

    /// `, @on_success_action = 4, @on_success_step_id = 3` (or the
    /// `@on_fail_` pair), the arguments `sp_add_jobstep` and
    /// `sp_update_jobstep` both take.
    fn arguments(self, prefix: &str) -> String {
        let mut sql = format!(", @{prefix}_action = {}", self.action_code());
        if let StepFlow::GoToStep(step_id) = self {
            sql.push_str(&format!(", @{prefix}_step_id = {step_id}"));
        }
        sql
    }
}

fn push_step_flow(sql: &mut String, step: &JobStepDefinition) {
    if let Some(flow) = step.on_success {
        sql.push_str(&flow.arguments("on_success"));
    }
    if let Some(flow) = step.on_failure {
        sql.push_str(&flow.arguments("on_fail"));
    }
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
            push_step_flow(&mut sql, step);
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
        push_step_flow(&mut sql, step);
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
            "SELECT s.step_id, s.step_name, s.subsystem, s.command, s.database_name, \
             s.on_success_action, s.on_success_step_id, s.on_fail_action, s.on_fail_step_id \
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
                on_success: None,
                on_failure: None,
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
            on_success: None,
            on_failure: None,
        };
        assert!(JobWriter::upsert_job_step_sql("Old", None, &step).contains("sp_add_jobstep"));
        let edit = JobWriter::upsert_job_step_sql("Old", Some(2), &step);
        assert!(edit.contains("sp_update_jobstep"));
        assert!(edit.contains("@step_id = 2"));
        // Unset flow is left to the server: an edit never resets it.
        assert!(!edit.contains("@on_success_action"), "{edit}");
        assert!(!edit.contains("@on_fail_action"), "{edit}");
    }

    #[test]
    fn step_flow_scripts_go_to_step_and_quit_actions() {
        let step = JobStepDefinition {
            step_name: "Load".into(),
            subsystem: "TSQL".into(),
            command: "SELECT 1".into(),
            database_name: None,
            on_success: Some(StepFlow::GoToStep(4)),
            on_failure: Some(StepFlow::QuitWithFailure),
        };
        let edit = JobWriter::upsert_job_step_sql("Nightly", Some(2), &step);
        assert!(
            edit.ends_with(
                ", @on_success_action = 4, @on_success_step_id = 4, @on_fail_action = 2;"
            ),
            "{edit}"
        );
        let next = JobStepDefinition {
            on_success: Some(StepFlow::NextStep),
            on_failure: Some(StepFlow::GoToStep(3)),
            ..step
        };
        let create = JobWriter::create_job_sql(
            &JobDefinition {
                name: "Nightly".into(),
                enabled: true,
                description: String::new(),
                owner_login: None,
            },
            &[next],
        );
        assert!(
            create.contains("@on_success_action = 3, @on_fail_action = 4, @on_fail_step_id = 3;\n"),
            "{create}"
        );
    }

    /// Regression: a step with no flow choice scripts exactly what it did
    /// before step flow existed, and step JSON written before the fields
    /// existed still parses.
    #[test]
    fn step_without_flow_scripts_no_flow_arguments() {
        let step: JobStepDefinition = serde_json::from_str(
            r#"{"step_name":"Load","subsystem":"TSQL","command":"SELECT 1","database_name":"app"}"#,
        )
        .unwrap();
        assert_eq!(step.on_success, None);
        assert_eq!(step.on_failure, None);
        let edit = JobWriter::upsert_job_step_sql("Nightly", Some(1), &step);
        assert_eq!(
            edit,
            "EXEC msdb.dbo.sp_update_jobstep @job_name = N'Nightly', @step_id = 1, \
             @step_name = N'Load', @subsystem = N'TSQL', @command = N'SELECT 1', \
             @database_name = N'app';"
        );
        assert!(!edit.contains("_action"), "{edit}");
    }

    #[test]
    fn step_flow_reads_catalog_codes() {
        assert_eq!(
            StepFlow::from_catalog(1, 0),
            Some(StepFlow::QuitWithSuccess)
        );
        assert_eq!(
            StepFlow::from_catalog(2, 0),
            Some(StepFlow::QuitWithFailure)
        );
        assert_eq!(StepFlow::from_catalog(3, 0), Some(StepFlow::NextStep));
        assert_eq!(StepFlow::from_catalog(4, 5), Some(StepFlow::GoToStep(5)));
        assert_eq!(StepFlow::from_catalog(4, 0), None);
        assert_eq!(StepFlow::from_catalog(9, 0), None);
        assert!(JobReader::job_steps_query("x").contains("s.on_fail_step_id"));
    }
}
