//! One-shot write grant for agent-proposed SQL.

use sqlly_core::{ConnectionId, ConnectionProfile};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteGrant {
    sql: String,
    connection: ConnectionId,
    database: Option<String>,
    consumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    SqlMismatch,
    ConnectionMismatch,
    DatabaseMismatch,
    AlreadyUsed,
}

impl WriteGrant {
    pub fn new(sql: &str, profile: &ConnectionProfile) -> Self {
        Self {
            sql: sql.trim().to_string(),
            connection: profile.id,
            database: profile.database.clone(),
            consumed: false,
        }
    }

    pub fn consume(&mut self, sql: &str, profile: &ConnectionProfile) -> Result<(), GrantError> {
        if self.consumed {
            return Err(GrantError::AlreadyUsed);
        }
        if sql.trim() != self.sql {
            return Err(GrantError::SqlMismatch);
        }
        if profile.id != self.connection {
            return Err(GrantError::ConnectionMismatch);
        }
        if profile.database != self.database {
            return Err(GrantError::DatabaseMismatch);
        }
        self.consumed = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    fn test_profile(id_seed: u8, database: Option<&str>) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            "grant-test",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        profile.id = ConnectionId(uuid::Uuid::from_u128(id_seed as u128));
        profile.database = database.map(str::to_string);
        profile
    }

    #[test]
    fn consume_twice_second_is_already_used() {
        let profile = test_profile(1, Some("app"));
        let mut grant = WriteGrant::new("UPDATE t SET x = 1", &profile);
        assert!(grant.consume("UPDATE t SET x = 1", &profile).is_ok());
        assert_eq!(
            grant.consume("UPDATE t SET x = 1", &profile),
            Err(GrantError::AlreadyUsed)
        );
    }

    #[test]
    fn whitespace_only_difference_is_ok() {
        let profile = test_profile(1, Some("app"));
        let mut grant = WriteGrant::new("  UPDATE t SET x = 1  ", &profile);
        assert!(grant.consume("UPDATE t SET x = 1", &profile).is_ok());
    }

    #[test]
    fn sql_change_is_mismatch() {
        let profile = test_profile(1, Some("app"));
        let mut grant = WriteGrant::new("UPDATE t SET x = 1", &profile);
        assert_eq!(
            grant.consume("UPDATE t SET x = 2", &profile),
            Err(GrantError::SqlMismatch)
        );
    }

    #[test]
    fn connection_mismatch() {
        let profile = test_profile(1, Some("app"));
        let other = test_profile(2, Some("app"));
        let mut grant = WriteGrant::new("UPDATE t SET x = 1", &profile);
        assert_eq!(
            grant.consume("UPDATE t SET x = 1", &other),
            Err(GrantError::ConnectionMismatch)
        );
    }

    #[test]
    fn database_mismatch() {
        let profile = test_profile(1, Some("app"));
        let other = test_profile(1, Some("other"));
        let mut grant = WriteGrant::new("UPDATE t SET x = 1", &profile);
        assert_eq!(
            grant.consume("UPDATE t SET x = 1", &other),
            Err(GrantError::DatabaseMismatch)
        );
    }
}
