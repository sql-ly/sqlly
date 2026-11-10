use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorInfo {
    pub id: i32,
    pub name: String,
    pub enabled: bool,
    pub email_address: Option<String>,
}

pub struct OperatorReader;

impl OperatorReader {
    pub fn list_operators_query() -> &'static str {
        "SELECT o.id, o.name, o.enabled, o.email_address \
         FROM msdb.dbo.sysoperators o \
         ORDER BY o.name"
    }
}

/// What an operator editor collects; maps 1:1 onto `sp_add_operator` /
/// `sp_update_operator` arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorDefinition {
    pub name: String,
    pub enabled: bool,
    pub email_address: Option<String>,
}

pub struct OperatorWriter;

impl OperatorWriter {
    pub fn create_operator_sql(operator: &OperatorDefinition) -> String {
        let mut sql = format!(
            "EXEC msdb.dbo.sp_add_operator @name = {}, @enabled = {}",
            quote_literal(&operator.name),
            i32::from(operator.enabled)
        );
        if let Some(email) = operator.email_address.as_deref().filter(|e| !e.is_empty()) {
            sql.push_str(&format!(", @email_address = {}", quote_literal(email)));
        }
        sql.push(';');
        sql
    }

    pub fn update_operator_sql(original_name: &str, operator: &OperatorDefinition) -> String {
        let mut sql = format!(
            "EXEC msdb.dbo.sp_update_operator @name = {}, @new_name = {}, @enabled = {}",
            quote_literal(original_name),
            quote_literal(&operator.name),
            i32::from(operator.enabled)
        );
        if let Some(email) = operator.email_address.as_deref().filter(|e| !e.is_empty()) {
            sql.push_str(&format!(", @email_address = {}", quote_literal(email)));
        }
        sql.push(';');
        sql
    }

    pub fn delete_operator_sql(name: &str) -> String {
        format!(
            "EXEC msdb.dbo.sp_delete_operator @name = {};",
            quote_literal(name)
        )
    }
}

pub struct AlertReader;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertDefinition {
    pub name: String,
    pub enabled: bool,
    pub message_id: i32,
    pub severity: i32,
    pub delay_between_responses: i32,
}

pub struct AlertWriter;

impl AlertWriter {
    pub fn create_alert_sql(alert: &AlertDefinition) -> String {
        format!(
            "EXEC msdb.dbo.sp_add_alert @name = {}, @enabled = {}, @message_id = {}, @severity = {}, @delay_between_responses = {};",
            quote_literal(&alert.name),
            i32::from(alert.enabled),
            alert.message_id.max(0),
            alert.severity.clamp(0, 25),
            alert.delay_between_responses.max(0)
        )
    }

    pub fn update_alert_sql(original_name: &str, alert: &AlertDefinition) -> String {
        format!(
            "EXEC msdb.dbo.sp_update_alert @name = {}, @new_name = {}, @enabled = {}, @message_id = {}, @severity = {}, @delay_between_responses = {};",
            quote_literal(original_name),
            quote_literal(&alert.name),
            i32::from(alert.enabled),
            alert.message_id.max(0),
            alert.severity.clamp(0, 25),
            alert.delay_between_responses.max(0)
        )
    }

    /// Wire an alert to an operator by email (`@notification_method = 1`),
    /// so firing the alert actually reaches someone.
    pub fn add_notification_sql(alert_name: &str, operator_name: &str) -> String {
        format!(
            "EXEC msdb.dbo.sp_add_notification @alert_name = {}, @operator_name = {}, @notification_method = 1;",
            quote_literal(alert_name),
            quote_literal(operator_name)
        )
    }

    pub fn delete_alert_sql(name: &str) -> String {
        format!(
            "EXEC msdb.dbo.sp_delete_alert @name = {};",
            quote_literal(name)
        )
    }
}

/// A parsed `sysalerts` row, with the condition rendered for humans instead
/// of raw code pairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertInfo {
    pub id: i32,
    pub name: String,
    pub enabled: bool,
    pub message_id: i32,
    pub severity: i32,
    pub delay_between_responses: i32,
    /// Human description of what fires the alert: a specific error number, a
    /// severity level, or neither.
    pub condition: String,
}

impl AlertReader {
    pub fn list_alerts_query() -> &'static str {
        "SELECT id, name, enabled, message_id, severity, delay_between_responses FROM msdb.dbo.sysalerts ORDER BY name"
    }

    /// Parse one positional row of [`Self::list_alerts_query`]'s shape into a
    /// typed alert. `None` when the row is malformed (missing id or name).
    pub fn parse_alert_row(
        id: Option<i64>,
        name: Option<&str>,
        enabled: Option<i64>,
        message_id: Option<i64>,
        severity: Option<i64>,
        delay_between_responses: Option<i64>,
    ) -> Option<AlertInfo> {
        let id = id? as i32;
        let name = name?.to_string();
        let message_id = message_id.unwrap_or(0) as i32;
        let severity = severity.unwrap_or(0) as i32;
        let condition = if message_id > 0 {
            format!("Error {message_id}")
        } else if severity > 0 {
            format!("Severity {severity}")
        } else {
            "Any qualifying event".to_string()
        };
        Some(AlertInfo {
            id,
            name,
            enabled: enabled.unwrap_or(0) != 0,
            message_id,
            severity,
            delay_between_responses: delay_between_responses.unwrap_or(0) as i32,
            condition,
        })
    }
}

fn quote_literal(value: &str) -> String {
    format!("N'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_operator_and_alert_queries() {
        assert!(OperatorReader::list_operators_query().contains("sysoperators"));
        assert!(AlertReader::list_alerts_query().contains("sysalerts"));
    }

    #[test]
    fn parses_alert_rows_with_readable_conditions() {
        let error_alert = AlertReader::parse_alert_row(
            Some(1),
            Some("deadlocks"),
            Some(1),
            Some(1205),
            Some(0),
            Some(60),
        )
        .unwrap();
        assert!(error_alert.enabled);
        assert_eq!(error_alert.condition, "Error 1205");

        let severity_alert =
            AlertReader::parse_alert_row(Some(2), Some("sev 17"), Some(0), Some(0), Some(17), None)
                .unwrap();
        assert!(!severity_alert.enabled);
        assert_eq!(severity_alert.condition, "Severity 17");

        assert!(AlertReader::parse_alert_row(None, Some("x"), None, None, None, None).is_none());
    }

    #[test]
    fn scripts_operator_create_edit_and_delete() {
        let operator = OperatorDefinition {
            name: "On-call DBA".into(),
            enabled: true,
            email_address: Some("oncall@example.com".into()),
        };
        let create = OperatorWriter::create_operator_sql(&operator);
        assert!(create.contains("sp_add_operator"));
        assert!(create.contains("@email_address = N'oncall@example.com'"));
        assert!(create.ends_with(';'));

        let no_email = OperatorDefinition {
            name: "O'Brien".into(),
            enabled: false,
            email_address: None,
        };
        let create = OperatorWriter::create_operator_sql(&no_email);
        assert!(create.contains("N'O''Brien'"));
        assert!(!create.contains("@email_address"));
        assert!(create.contains("@enabled = 0"));

        let update = OperatorWriter::update_operator_sql("old name", &operator);
        assert!(update.contains("sp_update_operator"));
        assert!(update.contains("@name = N'old name'"));
        assert!(update.contains("@new_name = N'On-call DBA'"));

        let delete = OperatorWriter::delete_operator_sql("On-call DBA");
        assert!(delete.contains("sp_delete_operator @name = N'On-call DBA';"));
    }

    #[test]
    fn scripts_alert_notification_and_delete() {
        let notify = AlertWriter::add_notification_sql("deadlocks", "On-call DBA");
        assert!(notify.contains("sp_add_notification"));
        assert!(notify.contains("@alert_name = N'deadlocks'"));
        assert!(notify.contains("@operator_name = N'On-call DBA'"));
        assert!(notify.contains("@notification_method = 1"));

        let delete = AlertWriter::delete_alert_sql("deadlocks");
        assert!(delete.contains("sp_delete_alert @name = N'deadlocks';"));
    }

    #[test]
    fn scripts_alert_create_and_edit_with_bounded_values() {
        let alert = AlertDefinition {
            name: "O'Brien deadlock".into(),
            enabled: true,
            message_id: 1205,
            severity: 99,
            delay_between_responses: 60,
        };
        let create = AlertWriter::create_alert_sql(&alert);
        assert!(create.contains("sp_add_alert"));
        assert!(create.contains("N'O''Brien deadlock'"));
        assert!(create.contains("@severity = 25"));
        let update = AlertWriter::update_alert_sql("old", &alert);
        assert!(update.contains("sp_update_alert"));
        assert!(update.contains("@new_name"));
    }
}
