//! The SQL Server transport contract ([`SqlServerTransport`]), its concrete
//! `tiberius`-backed implementation ([`TiberiusTransport`]), and the
//! event-publishing [`ConnectionService`] wrapper.

use async_trait::async_trait;
use sqlly_core::{
    AuthenticationType, ConnectionProfile, DatabaseSummary, EventBus, EventPayload, RowSender,
    SecretProvider, StudioResult,
};

use crate::tds::client::{connect, open_client};
use crate::tds::error::to_query_error;
use crate::tds::execute::execute_on_client;

#[async_trait]
pub trait SqlServerTransport: Send + Sync {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()>;
    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>>;
    async fn execute_query(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        events: &EventBus,
        rows_tx: Option<&RowSender>,
    ) -> StudioResult<()>;
}

#[derive(Debug, Clone, Default)]
pub struct TiberiusTransport;

/// The engine-neutral transport contract every driver crate implements
/// (`crate::driver::SqlTransport`), so kind-dispatching consumers can hold TDS
/// alongside the PostgreSQL/MySQL/SQLite transports behind one trait.
/// Delegates to the [`SqlServerTransport`] methods below.
#[async_trait]
impl crate::driver::SqlTransport for TiberiusTransport {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        SqlServerTransport::test_connection(self, profile).await
    }

    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        SqlServerTransport::list_databases(self, profile).await
    }

    async fn execute_query(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        events: &EventBus,
        rows_tx: Option<&RowSender>,
    ) -> StudioResult<()> {
        SqlServerTransport::execute_query(self, profile, sql, events, rows_tx).await
    }
}

#[async_trait]
impl SqlServerTransport for TiberiusTransport {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        let _client = open_client(profile).await?;
        Ok(())
    }

    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        let mut client = open_client(profile).await?;
        let rows = client
            .query(
                "SELECT name, state_desc, compatibility_level FROM sys.databases ORDER BY name",
                &[],
            )
            .await
            .map_err(to_query_error)?
            .into_first_result()
            .await
            .map_err(to_query_error)?;

        rows.into_iter()
            .map(|row| {
                let name = row
                    .try_get::<&str, _>(0)
                    .map_err(to_query_error)?
                    .unwrap_or_default()
                    .to_string();
                let state = row
                    .try_get::<&str, _>(1)
                    .map_err(to_query_error)?
                    .unwrap_or_default()
                    .to_string();
                // `sys.databases.compatibility_level` is `tinyint`, which the
                // TDS driver surfaces as `u8` (not `i32`); read it as such.
                let compatibility_level = row
                    .try_get::<u8, _>(2)
                    .map_err(to_query_error)?
                    .map(u16::from);
                Ok(DatabaseSummary {
                    name,
                    state,
                    compatibility_level,
                })
            })
            .collect()
    }

    async fn execute_query(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        events: &EventBus,
        rows_tx: Option<&RowSender>,
    ) -> StudioResult<()> {
        let mut client = connect(profile).await?;
        execute_on_client(&mut client, profile, sql, events, rows_tx)
            .await
            .map(|_| ())
    }
}

#[derive(Clone)]
pub struct ConnectionService<T> {
    transport: T,
    events: EventBus,
}

impl<T> ConnectionService<T>
where
    T: SqlServerTransport,
{
    pub fn new(transport: T, events: EventBus) -> Self {
        Self { transport, events }
    }

    pub async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        let _ = self.events.publish(EventPayload::ConnectionTesting {
            connection_id: profile.id,
        });
        self.transport.test_connection(profile).await?;
        self.events.publish(EventPayload::ConnectionOpened {
            connection_id: profile.id,
        })?;
        Ok(())
    }

    pub fn disconnect(&self, connection_id: sqlly_core::ConnectionId) -> StudioResult<()> {
        self.events
            .publish(EventPayload::ConnectionClosed { connection_id })?;
        Ok(())
    }

    pub async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        let databases = self.transport.list_databases(profile).await?;
        self.events.publish(EventPayload::DatabasesListed {
            connection_id: profile.id,
            databases: databases.clone(),
        })?;
        Ok(databases)
    }
}

/// Plain (non-tiberius) connection settings extracted from a profile; used by
/// alternative transports and tooling that need the raw endpoint fields.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub server: String,
    pub port: u16,
    pub database: Option<String>,
    pub encrypt: bool,
    pub trust_certificate: bool,
    pub connect_timeout: std::time::Duration,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl TransportConfig {
    pub fn from_profile(profile: &ConnectionProfile) -> Self {
        let (username, password) = match &profile.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => {
                let pwd = match secret_ref.provider {
                    SecretProvider::Environment => std::env::var(&secret_ref.key).ok(),
                    _ => None,
                };
                (Some(username.clone()), pwd)
            }
            _ => (None, None),
        };
        Self {
            server: profile.server.clone(),
            port: profile.port,
            database: profile.database.clone(),
            encrypt: profile.encrypt,
            trust_certificate: profile.trust_server_certificate,
            connect_timeout: profile.timeout(),
            username,
            password,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{
        CellValue, ResultColumn, RowBatch, SecretProvider, SecretRef, StudioError,
        event::EventPayload,
    };
    use tokio::sync::broadcast::error::TryRecvError;

    #[derive(Clone)]
    struct MockTransport;

    #[async_trait]
    impl SqlServerTransport for MockTransport {
        async fn test_connection(&self, _profile: &ConnectionProfile) -> StudioResult<()> {
            Ok(())
        }

        async fn list_databases(
            &self,
            _profile: &ConnectionProfile,
        ) -> StudioResult<Vec<DatabaseSummary>> {
            Ok(vec![DatabaseSummary {
                name: "master".to_string(),
                state: "ONLINE".to_string(),
                compatibility_level: Some(160),
            }])
        }

        async fn execute_query(
            &self,
            _profile: &ConnectionProfile,
            _sql: &str,
            events: &EventBus,
            rows_tx: Option<&RowSender>,
        ) -> StudioResult<()> {
            let session_id = sqlly_core::SessionId::new();
            events.publish(EventPayload::QueryStarted {
                session_id: session_id.clone(),
            })?;
            events.publish(EventPayload::ResultSetStarted {
                session_id: session_id.clone(),
                result_set_index: 0,
                columns: vec![ResultColumn {
                    name: "value".to_string(),
                    data_type: "int".to_string(),
                    nullable: false,
                }],
            })?;
            match rows_tx {
                Some(tx) => {
                    tx.send(RowBatch::new(
                        session_id.clone(),
                        0,
                        vec![vec![CellValue::I64(1)]],
                    ))
                    .await
                    .map_err(|e| StudioError::InvalidInput(e.to_string()))?;
                }
                None => {
                    events.publish(EventPayload::Rows {
                        session_id: session_id.clone(),
                        result_set_index: 0,
                        rows: vec![vec![CellValue::I64(1)]],
                    })?;
                }
            }
            events.publish(EventPayload::RowCount {
                session_id: session_id.clone(),
                rows_affected: 1,
            })?;
            events.publish(EventPayload::QueryCompleted {
                session_id,
                elapsed_ms: 1,
            })?;
            Ok(())
        }
    }

    fn profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "password".to_string(),
            },
        )
    }

    fn drain_payloads(events: &mut sqlly_core::EventSubscription) -> Vec<EventPayload> {
        let mut payloads = Vec::new();
        loop {
            match events.try_recv() {
                Ok(event) => payloads.push(event.payload),
                Err(TryRecvError::Empty) => break,
                Err(error) => panic!("unexpected event error: {error}"),
            }
        }
        payloads
    }

    #[tokio::test]
    async fn connection_service_publishes_connection_lifecycle_events() {
        let events = EventBus::default();
        let mut subscription = events.subscribe();
        let service = ConnectionService::new(MockTransport, events);
        let profile = profile();

        service.test_connection(&profile).await.unwrap();
        let databases = service.list_databases(&profile).await.unwrap();
        service.disconnect(profile.id).unwrap();

        assert_eq!(databases.len(), 1);
        let payloads = drain_payloads(&mut subscription);
        assert!(matches!(
            payloads[0],
            EventPayload::ConnectionTesting { .. }
        ));
        assert!(matches!(payloads[1], EventPayload::ConnectionOpened { .. }));
        assert!(matches!(payloads[2], EventPayload::DatabasesListed { .. }));
        assert!(matches!(payloads[3], EventPayload::ConnectionClosed { .. }));
    }

    #[tokio::test]
    async fn mock_query_transport_streams_stable_result_events() {
        let events = EventBus::default();
        let mut subscription = events.subscribe();

        MockTransport
            .execute_query(&profile(), "SELECT 1", &events, None)
            .await
            .unwrap();

        let payloads = drain_payloads(&mut subscription);
        assert!(matches!(payloads[0], EventPayload::QueryStarted { .. }));
        assert!(matches!(payloads[1], EventPayload::ResultSetStarted { .. }));
        assert!(matches!(payloads[2], EventPayload::Rows { .. }));
        assert!(matches!(payloads[3], EventPayload::RowCount { .. }));
        assert!(matches!(payloads[4], EventPayload::QueryCompleted { .. }));
    }

    /// When a [`RowSender`] is supplied, rows flow over the bounded mpsc channel
    /// (not the broadcast bus) while lifecycle events still flow over the bus —
    /// this is the end-to-end backpressured query/row path the CLI uses.
    #[tokio::test]
    async fn execute_query_routes_rows_over_bounded_channel_when_provided() {
        let events = EventBus::default();
        let mut subscription = events.subscribe();
        let (tx, mut rx) = sqlly_core::row_channel(8);

        MockTransport
            .execute_query(&profile(), "SELECT 1", &events, Some(&tx))
            .await
            .unwrap();
        drop(tx);

        // Lifecycle events on the bus contain NO Rows payload.
        let payloads = drain_payloads(&mut subscription);
        assert!(
            !payloads
                .iter()
                .any(|p| matches!(p, EventPayload::Rows { .. })),
            "rows must not be published on the bus when a channel is provided"
        );
        assert!(
            payloads
                .iter()
                .any(|p| matches!(p, EventPayload::QueryStarted { .. }))
        );
        assert!(
            payloads
                .iter()
                .any(|p| matches!(p, EventPayload::QueryCompleted { .. }))
        );

        // The row arrived over the bounded channel.
        let mut total = 0usize;
        while let Some(batch) = rx.recv().await {
            total += batch.len();
        }
        assert_eq!(total, 1);
    }
}
