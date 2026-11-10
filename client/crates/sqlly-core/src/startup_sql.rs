//! Per-connection startup SQL helpers: trim, dialect-aware batch splitting,
//! and the connection error prefix drivers use when a batch fails.

use crate::{ConnectionProfile, StudioError};
use sqlly_lang::sqllex::split_batches;

/// Non-empty startup SQL configured on the profile, if any.
pub fn startup_sql_text(profile: &ConnectionProfile) -> Option<&str> {
    profile
        .startup_sql
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// Split configured startup SQL into dialect-aware execution batches.
pub fn startup_sql_batches(profile: &ConnectionProfile) -> Option<Vec<&str>> {
    let text = startup_sql_text(profile)?;
    let dialect = profile.database_type.family();
    Some(
        split_batches(text, dialect)
            .into_iter()
            .map(|batch| batch.text)
            .collect(),
    )
}

/// Wrap a startup execution failure in the connection error the UI surfaces.
pub fn startup_sql_failed(err: impl std::fmt::Display) -> StudioError {
    StudioError::Connection(format!("Startup SQL failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SecretProvider, SecretRef};
    use sqlly_lang::sqlmodel::DatabaseType;

    fn profile_with_startup(sql: Option<&str>) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.startup_sql = sql.map(str::to_string);
        profile
    }

    #[test]
    fn empty_and_whitespace_startup_sql_is_absent() {
        assert!(startup_sql_text(&profile_with_startup(None)).is_none());
        assert!(startup_sql_text(&profile_with_startup(Some(""))).is_none());
        assert!(startup_sql_text(&profile_with_startup(Some("  \n  "))).is_none());
        assert!(startup_sql_batches(&profile_with_startup(Some("  "))).is_none());
    }

    #[test]
    fn sqlserver_startup_sql_splits_on_go() {
        let mut profile = profile_with_startup(Some("SET NOCOUNT ON;\nGO\nSET ANSI_NULLS ON;"));
        profile.database_type = DatabaseType::SqlServer;
        let batches = startup_sql_batches(&profile).expect("batches");
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0], "SET NOCOUNT ON;");
        assert_eq!(batches[1], "SET ANSI_NULLS ON;");
    }

    #[test]
    fn postgres_startup_sql_stays_one_batch_with_semicolons() {
        let mut profile = profile_with_startup(Some(
            "SET search_path = public; SET client_encoding = 'UTF8';",
        ));
        profile.database_type = DatabaseType::Postgres;
        let batches = startup_sql_batches(&profile).expect("batches");
        assert_eq!(batches.len(), 1);
        assert!(batches[0].contains("search_path"));
        assert!(batches[0].contains("client_encoding"));
    }

    #[test]
    fn startup_sql_failed_prefixes_connection_error() {
        let err = startup_sql_failed("invalid object name 'missing'");
        match err {
            StudioError::Connection(msg) => {
                assert_eq!(msg, "Startup SQL failed: invalid object name 'missing'");
            }
            other => panic!("expected Connection error, got {other:?}"),
        }
    }
}
