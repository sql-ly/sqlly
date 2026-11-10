use sqlly_core::ConnectionProfile;

/// What the explorer's double-click must do to make `target` the active
/// connection before a query tab bound to it may open.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RetargetDecision {
    /// Target is already the active server and database.
    AlreadyActive,
    /// Same server, different database — a synchronous database switch.
    SwitchDatabase(String),
    /// Different (or no) active server — a full, possibly async, connect.
    Connect,
}

pub(crate) fn retarget_decision(
    active: Option<&ConnectionProfile>,
    target: &ConnectionProfile,
) -> RetargetDecision {
    let Some(active) = active else {
        return RetargetDecision::Connect;
    };
    if active.id.0 != target.id.0 {
        return RetargetDecision::Connect;
    }
    match target.database.clone() {
        Some(db) if active.database.as_deref() != Some(db.as_str()) => {
            RetargetDecision::SwitchDatabase(db)
        }
        _ => RetargetDecision::AlreadyActive,
    }
}

#[cfg(test)]
mod retarget_tests {
    use super::{RetargetDecision, retarget_decision};

    fn profile(database: &str) -> sqlly_core::ConnectionProfile {
        sqlly_core::ConnectionProfile::azure_sql_entra_browser(
            "Warehouse",
            "warehouse.database.windows.net",
            database,
        )
    }

    #[test]
    fn no_active_connection_requires_connect() {
        let target = profile("master");
        assert_eq!(retarget_decision(None, &target), RetargetDecision::Connect);
    }

    #[test]
    fn different_server_requires_connect() {
        let active = profile("master");
        let target = profile("master"); // fresh profile ⇒ new id ⇒ other server
        assert_eq!(
            retarget_decision(Some(&active), &target),
            RetargetDecision::Connect
        );
    }

    #[test]
    fn same_server_different_database_switches() {
        let active = profile("master");
        let mut target = active.clone();
        target.database = Some("Reporting".to_string());
        assert_eq!(
            retarget_decision(Some(&active), &target),
            RetargetDecision::SwitchDatabase("Reporting".to_string())
        );
    }

    #[test]
    fn same_server_same_database_is_already_active() {
        let active = profile("master");
        let target = active.clone();
        assert_eq!(
            retarget_decision(Some(&active), &target),
            RetargetDecision::AlreadyActive
        );
    }

    #[test]
    fn target_without_database_keeps_active_connection() {
        let active = profile("master");
        let mut target = active.clone();
        target.database = None;
        assert_eq!(
            retarget_decision(Some(&active), &target),
            RetargetDecision::AlreadyActive
        );
    }
}
