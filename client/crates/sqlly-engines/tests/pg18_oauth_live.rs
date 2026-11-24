//! Live PostgreSQL 18 OAuth (SASL OAUTHBEARER) against a real server.
//!
//! Ignored by default. Start the container described in
//! `tests/fixtures/pg18_oauth/Dockerfile`, then run
//! `SQLLY_PG18_OAUTH_PORT=55418 cargo test -p sqlly-engines --test pg18_oauth_live -- --ignored`.

use sqlly_core::{
    AuthenticationType, ConnectionProfile, DatabaseType, EventBus, PgOAuthFlow, SecretProvider,
    SecretRef, StudioError, TlsMode,
};
use sqlly_engines::pg::{discover_oauth, execute_on_client, open_client};

fn port() -> u16 {
    std::env::var("SQLLY_PG18_OAUTH_PORT")
        .expect("SQLLY_PG18_OAUTH_PORT (see tests/fixtures/pg18_oauth/Dockerfile)")
        .parse()
        .expect("port number")
}

fn oauth_profile(user: &str, token: Option<&str>) -> ConnectionProfile {
    let mut p = ConnectionProfile::postgres_oauth("pg18-oauth", "127.0.0.1", port(), user);
    p.database = Some("postgres".into());
    // The container's certificate is self-signed.
    p.tls_mode = TlsMode::Require;
    p.runtime_cloud_iam_token = token.map(str::to_string);
    p
}

fn auth_key(error: StudioError) -> Option<String> {
    match error {
        StudioError::AuthenticationMessage(message) => message.key().map(str::to_string),
        other => panic!("expected an authentication message, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "needs the PostgreSQL 18 OAuth container"]
async fn live_pg18_signs_in_with_a_bearer_token_and_runs_a_query() {
    let client = open_client(&oauth_profile("alice", Some("good-token")))
        .await
        .expect("OAuth sign-in");
    let events = EventBus::new(64);
    let _sub = events.subscribe();
    execute_on_client(
        &client,
        &oauth_profile("alice", Some("good-token")),
        "SELECT current_user;",
        &events,
        None,
    )
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs the PostgreSQL 18 OAuth container"]
async fn live_pg18_stored_bearer_token_flow_signs_in() {
    let mut p = oauth_profile("alice", None);
    p.authentication = AuthenticationType::PostgresOAuth {
        username: "alice".into(),
        issuer: String::new(),
        client_id: String::new(),
        scope: String::new(),
        flow: PgOAuthFlow::BearerToken,
        secret_ref: SecretRef {
            provider: SecretProvider::InMemory,
            key: "good-token".into(),
        },
    };
    open_client(&p).await.expect("stored bearer token signs in");
}

#[tokio::test]
#[ignore = "needs the PostgreSQL 18 OAuth container"]
async fn live_pg18_rejects_a_bad_token_readably() {
    let error = open_client(&oauth_profile("alice", Some("bad-token")))
        .await
        .err()
        .expect("bad token is refused");
    assert_eq!(
        auth_key(error).as_deref(),
        Some("services.pg.oauth_token_rejected")
    );
}

#[tokio::test]
#[ignore = "needs the PostgreSQL 18 OAuth container"]
async fn live_pg18_discovery_names_the_issuer_and_scope() {
    let hint = discover_oauth(&oauth_profile("alice", None))
        .await
        .expect("discovery document");
    assert_eq!(hint.status.as_deref(), Some("invalid_token"));
    assert_eq!(
        hint.openid_configuration.as_deref(),
        Some("https://idp.example.com/.well-known/openid-configuration")
    );
    assert_eq!(hint.scope.as_deref(), Some("openid postgres"));
}

#[tokio::test]
#[ignore = "needs the PostgreSQL 18 OAuth container"]
async fn live_pg18_password_role_reports_oauth_not_offered() {
    let error = open_client(&oauth_profile("bob", Some("good-token")))
        .await
        .err()
        .expect("bob signs in with a password");
    assert_eq!(
        auth_key(error).as_deref(),
        Some("services.pg.oauth_not_offered")
    );
    let mut p = oauth_profile("bob", None);
    p.database_type = DatabaseType::Postgres;
    assert_eq!(
        auth_key(discover_oauth(&p).await.unwrap_err()).as_deref(),
        Some("services.pg.oauth_not_offered")
    );
}
