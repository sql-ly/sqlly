//! Live-server integration check for the completion pipeline — the exact
//! path the GPUI app runs on connect: `DefaultQueryExecutor` →
//! `DefaultMetadataService` → `build_completion_state` →
//! `complete_enriched_response`.
//!
//! Self-skips when no live SQL config is present (see `sqlly-testkit`), so a
//! vanilla checkout stays hermetic. With a config present it asserts that a
//! real connection produces a catalog containing tables and that the
//! completion engine surfaces them — diagnosing "intellisense shows nothing
//! from the data model" style regressions with stage-by-stage timing.

use std::sync::Arc;
use std::time::Instant;

use sqlly_services::{
    CompletionRequest, DefaultMetadataService, DefaultQueryExecutor, IntelliSenseRuleConfig,
    IntellisenseService, build_completion_state,
};
use sqlly_testkit::LiveSqlConfig;

#[tokio::test(flavor = "multi_thread")]
async fn live_connection_feeds_data_model_completions() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let profile = cfg.to_profile("live-completion");
    let database = cfg.database.clone();

    let queries: Arc<dyn sqlly_services::QueryExecutor> = Arc::new(DefaultQueryExecutor::new());
    let metadata = DefaultMetadataService::new(queries);

    let started = Instant::now();
    let state = match build_completion_state(&metadata, &profile, &database, false).await {
        Ok(state) => state,
        Err(e) => panic!("build_completion_state failed against live server: {e}"),
    };
    let elapsed = started.elapsed();

    let tables = state
        .catalog
        .objects
        .iter()
        .filter(|o| o.kind == sqlly_core::ObjectKind::Table)
        .count();
    let with_columns = state
        .catalog
        .objects
        .iter()
        .filter(|o| !o.columns.is_empty())
        .count();
    eprintln!(
        "build_completion_state: {:?} — {} objects ({} tables, {} with columns), \
         {} schemas, {} procedures, snapshot len {}",
        elapsed,
        state.catalog.objects.len(),
        tables,
        with_columns,
        state.catalog.schemas.len(),
        state.procedures.len(),
        state.snapshot.len(),
    );

    assert!(
        !state.catalog.objects.is_empty(),
        "live catalog has no objects — the data model is empty"
    );

    // Drive the same completion call the editor makes.
    let sql = "SELECT * FROM ";
    let rules = IntelliSenseRuleConfig::default();
    let req = CompletionRequest::auto(sql, sql.len(), &state.catalog, &state.procedures, &rules);
    let svc = sqlly_services::DefaultIntellisense;
    let response = svc.complete_enriched_response(&req);
    let table_items: Vec<&str> = response
        .items
        .iter()
        .filter(|i| i.kind == sqlly_intel::intellisense::CompletionKind::Table)
        .map(|i| i.label.as_str())
        .take(10)
        .collect();
    eprintln!(
        "completion after FROM: {} items, should_render={}, first tables: {:?}",
        response.items.len(),
        response.should_render,
        table_items
    );
    assert!(
        response
            .items
            .iter()
            .any(|i| i.kind == sqlly_intel::intellisense::CompletionKind::Table),
        "no table completions from the live data model; got: {:?}",
        response
            .items
            .iter()
            .map(|i| &i.label)
            .take(20)
            .collect::<Vec<_>>()
    );
}
