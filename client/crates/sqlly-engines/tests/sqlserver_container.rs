use std::time::Duration;

use sqlly_core::{EventBus, EventPayload};
use sqlly_engines::tds::{SqlServerTransport, TiberiusTransport};
use sqlly_testkit::SqlServerContainer;

const SA_PASSWORD: &str = "Sqlly!Sql2026Container";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker and pulls/runs mcr.microsoft.com/mssql/server"]
async fn validates_execution_cancellation_and_multi_result_sets_against_sql_server_container() {
    let run_container = std::env::var("SQLLY_RUN_SQLSERVER_CONTAINER")
        .or_else(|_| std::env::var("OSXQL_RUN_SQLSERVER_CONTAINER"));
    if run_container.as_deref() != Ok("1") {
        eprintln!("set SQLLY_RUN_SQLSERVER_CONTAINER=1 to run the SQL Server container test");
        return;
    }

    let container = SqlServerContainer::start(SA_PASSWORD);
    let profile = wait_for_profile(&container).await;
    let transport = TiberiusTransport;

    transport.test_connection(&profile).await.unwrap();

    let events = EventBus::default();
    let mut subscription = events.subscribe();
    let (rows_tx, mut rows_rx) = sqlly_core::row_channel(16);
    let drain = tokio::spawn(async move {
        let mut total = 0usize;
        while let Some(batch) = rows_rx.recv().await {
            total += batch.len();
        }
        total
    });
    transport
        .execute_query(
            &profile,
            "SELECT 1 AS first_result; SELECT 2 AS second_result;",
            &events,
            Some(&rows_tx),
        )
        .await
        .unwrap();
    drop(rows_tx);
    let row_count = drain.await.unwrap();
    let payloads = drain_payloads(&mut subscription);
    let result_set_count = payloads
        .iter()
        .filter(|payload| matches!(payload, EventPayload::ResultSetStarted { .. }))
        .count();
    assert_eq!(result_set_count, 2, "payloads: {payloads:?}");
    assert_eq!(row_count, 2, "rows received over bounded channel");

    let cancel_events = EventBus::default();
    let cancel_profile = profile.clone();
    let cancel_task = tokio::spawn(async move {
        let (cancel_tx, _cancel_rx) = sqlly_core::row_channel(16);
        TiberiusTransport
            .execute_query(
                &cancel_profile,
                "WAITFOR DELAY '00:00:10'; SELECT 99 AS should_not_finish;",
                &cancel_events,
                Some(&cancel_tx),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    cancel_task.abort();
    assert!(cancel_task.await.unwrap_err().is_cancelled());
}

fn drain_payloads(subscription: &mut sqlly_core::EventSubscription) -> Vec<EventPayload> {
    let mut payloads = Vec::new();
    while let Ok(event) = subscription.try_recv() {
        payloads.push(event.payload);
    }
    payloads
}

async fn wait_for_profile(container: &SqlServerContainer) -> sqlly_core::ConnectionProfile {
    let profile = container.profile();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        if TiberiusTransport.test_connection(&profile).await.is_ok() {
            return profile;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "SQL Server container did not become ready"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
