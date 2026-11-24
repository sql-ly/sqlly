//! PostgreSQL 18 OAuth sign-in (SASL `OAUTHBEARER`, RFC 7628).
//!
//! The SASL exchange itself lives in the vendored tokio-postgres
//! (`client/vendor/tokio-postgres`, `tokio_postgres::oauth`); this module
//! maps a [`AuthenticationType::PostgresOAuth`] profile onto the driver
//! config, turns the driver's OAuth failures into readable errors, and reads
//! the server's discovery document (the JSON a PostgreSQL 18 server sends
//! back when it rejects a token or is asked for discovery with an empty one).

use serde::Deserialize;
use sqlly_core::{
    AuthenticationType, ConnectionProfile, DisplayMessage, PgOAuthFlow, StudioError, StudioResult,
};
pub use tokio_postgres::oauth::OAuthBearerError;

/// What a PostgreSQL 18 server advertises about its OAuth provider when it
/// rejects a token (RFC 7628 §3.2.2 error document).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PgOAuthServerHint {
    /// `invalid_token`, `invalid_request`, …
    #[serde(default)]
    pub status: Option<String>,
    /// The issuer's OpenID discovery document URL.
    #[serde(default, rename = "openid-configuration")]
    pub openid_configuration: Option<String>,
    /// Space-separated scopes the server wants on the token.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Parse the server's JSON error document. `None` when it is not JSON.
pub fn parse_server_hint(document: &str) -> Option<PgOAuthServerHint> {
    serde_json::from_str(document).ok()
}

/// The OAuth failure behind a driver error, if it is one.
pub fn oauth_error(error: &tokio_postgres::Error) -> Option<&OAuthBearerError> {
    std::error::Error::source(error).and_then(|source| source.downcast_ref::<OAuthBearerError>())
}

/// Configure `config` for an OAuth profile: the role in the startup packet
/// and the bearer token for `OAUTHBEARER`. The token comes from the runtime
/// slot (minted by the app's device-code flow, or `--access-token`), or from
/// the stored secret for the bearer-token flow.
pub(crate) fn apply_to_config(
    config: &mut tokio_postgres::Config,
    profile: &ConnectionProfile,
) -> StudioResult<()> {
    let AuthenticationType::PostgresOAuth {
        username,
        flow,
        secret_ref,
        ..
    } = &profile.authentication
    else {
        return Ok(());
    };
    config.user(username);
    let runtime = profile.runtime_cloud_iam_token.clone();
    let token = match flow {
        // A runtime token (from --access-token, or an empty discovery probe)
        // wins over the stored one.
        PgOAuthFlow::BearerToken => match runtime {
            Some(token) => token,
            None if secret_ref.is_empty_in_memory() => return Err(token_missing()),
            None => secret_ref.resolve()?.trim().to_string(),
        },
        PgOAuthFlow::DeviceCode => runtime.ok_or_else(token_missing)?,
    };
    config.oauth_bearer_token(token);
    Ok(())
}

fn token_missing() -> StudioError {
    StudioError::AuthenticationMessage(DisplayMessage::new(
        "services.pg.oauth_token_missing",
        "Authentication failed: no OAuth token for this connection; connect from SQLLY to sign in (or pass --access-token for the CLI)",
    ))
}

/// A readable error for an OAuth failure reported by the driver.
pub(crate) fn connection_error(error: &OAuthBearerError) -> StudioError {
    match error {
        OAuthBearerError::NotOffered { .. } => {
            StudioError::AuthenticationMessage(DisplayMessage::new(
                "services.pg.oauth_not_offered",
                "Authentication failed: the server did not offer OAuth sign-in for this user and database. OAuth needs PostgreSQL 18 or later with an oauth line in pg_hba.conf for this connection.",
            ))
        }
        OAuthBearerError::Rejected {
            server_response,
            server_message,
        } => {
            let status = parse_server_hint(server_response)
                .and_then(|hint| hint.status)
                .unwrap_or_else(|| "invalid_token".to_string());
            let detail = server_message.clone().unwrap_or_default();
            StudioError::AuthenticationMessage(
                DisplayMessage::new(
                    "services.pg.oauth_token_rejected",
                    format!("Authentication failed: the server rejected the OAuth token ({status}). {detail}"),
                )
                .with_arg("status", status)
                .with_arg("detail", detail),
            )
        }
    }
}

/// Turn the outcome of a discovery connect (one made with an empty token)
/// into the server's OAuth hint.
pub(crate) fn discovery_from_error(
    error: &tokio_postgres::Error,
) -> StudioResult<PgOAuthServerHint> {
    match oauth_error(error) {
        Some(OAuthBearerError::Rejected {
            server_response, ..
        }) => parse_server_hint(server_response)
            .filter(|hint| hint.openid_configuration.is_some())
            .ok_or_else(discovery_unavailable),
        Some(other) => Err(connection_error(other)),
        None => Err(StudioError::Authentication(error.to_string())),
    }
}

pub(crate) fn discovery_unavailable() -> StudioError {
    StudioError::AuthenticationMessage(DisplayMessage::new(
        "services.pg.oauth_discovery_unavailable",
        "The server did not say which OAuth provider to use; enter the issuer URL in the connection editor.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const DISCOVERY: &str = r#"{"status":"invalid_token","openid-configuration":"https://idp.example.com/.well-known/openid-configuration","scope":"openid postgres"}"#;

    fn oauth_profile(flow: PgOAuthFlow, secret: &str) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "pg18",
            "127.0.0.1",
            "alice",
            SecretRef::empty_in_memory(),
        );
        p.database_type = sqlly_core::DatabaseType::Postgres;
        p.port = 5432;
        p.database = Some("appdb".into());
        p.authentication = AuthenticationType::PostgresOAuth {
            username: "alice".into(),
            issuer: String::new(),
            client_id: "sqlly".into(),
            scope: String::new(),
            flow,
            secret_ref: SecretRef {
                provider: SecretProvider::InMemory,
                key: secret.into(),
            },
        };
        p
    }

    fn config_for(p: &ConnectionProfile) -> StudioResult<tokio_postgres::Config> {
        super::super::config_from_profile(p, "127.0.0.1", 5432)
    }

    // ---- framing ---------------------------------------------------------

    #[test]
    fn client_first_message_carries_the_bearer_token() {
        assert_eq!(
            tokio_postgres::oauth::client_initial_response("eyJ.abc"),
            b"n,,\x01auth=Bearer eyJ.abc\x01\x01".to_vec()
        );
    }

    #[test]
    fn empty_token_asks_the_server_for_discovery() {
        assert_eq!(
            tokio_postgres::oauth::client_initial_response(""),
            b"n,,\x01auth=\x01\x01".to_vec()
        );
        assert_eq!(
            tokio_postgres::oauth::client_error_acknowledgement(),
            vec![0x01]
        );
    }

    #[test]
    fn discovery_document_parses() {
        let hint = parse_server_hint(DISCOVERY).unwrap();
        assert_eq!(hint.status.as_deref(), Some("invalid_token"));
        assert_eq!(
            hint.openid_configuration.as_deref(),
            Some("https://idp.example.com/.well-known/openid-configuration")
        );
        assert_eq!(hint.scope.as_deref(), Some("openid postgres"));
        assert_eq!(
            parse_server_hint(r#"{"status":"invalid_request"}"#),
            Some(PgOAuthServerHint {
                status: Some("invalid_request".into()),
                ..Default::default()
            })
        );
        assert_eq!(parse_server_hint("not json"), None);
    }

    // ---- profile wiring --------------------------------------------------

    #[test]
    fn device_flow_sends_the_runtime_token_and_no_password() {
        let mut p = oauth_profile(PgOAuthFlow::DeviceCode, "client-secret");
        p.runtime_cloud_iam_token = Some("access".into());
        let config = config_for(&p).unwrap();
        assert_eq!(config.get_user(), Some("alice"));
        assert_eq!(config.get_oauth_bearer_token(), Some("access"));
        assert_eq!(config.get_password(), None);
    }

    #[test]
    fn device_flow_without_a_token_is_a_readable_authentication_error() {
        let p = oauth_profile(PgOAuthFlow::DeviceCode, "");
        let Err(StudioError::AuthenticationMessage(message)) = config_for(&p) else {
            panic!("expected an authentication message");
        };
        assert_eq!(message.key(), Some("services.pg.oauth_token_missing"));
    }

    #[test]
    fn bearer_flow_uses_the_stored_token_unless_a_runtime_one_is_given() {
        let mut p = oauth_profile(PgOAuthFlow::BearerToken, " stored ");
        assert_eq!(
            config_for(&p).unwrap().get_oauth_bearer_token(),
            Some("stored")
        );
        p.runtime_cloud_iam_token = Some(String::new());
        assert_eq!(config_for(&p).unwrap().get_oauth_bearer_token(), Some(""));
        let empty = oauth_profile(PgOAuthFlow::BearerToken, "");
        assert!(matches!(
            config_for(&empty),
            Err(StudioError::AuthenticationMessage(_))
        ));
    }

    #[test]
    fn oauth_profiles_always_use_tls() {
        let mut p = oauth_profile(PgOAuthFlow::DeviceCode, "");
        p.encrypt = false;
        p.tls_mode = sqlly_core::TlsMode::Disable;
        assert_ne!(
            super::super::tls_mode_for_profile(&p),
            sqlly_core::TlsMode::Disable
        );
    }

    // ---- a scripted PostgreSQL 18 server ---------------------------------

    fn auth_message(code: i32, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![b'R'];
        out.extend_from_slice(&(8 + payload.len() as i32).to_be_bytes());
        out.extend_from_slice(&code.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn sasl_offer(mechanisms: &[&str]) -> Vec<u8> {
        let mut payload = Vec::new();
        for mechanism in mechanisms {
            payload.extend_from_slice(mechanism.as_bytes());
            payload.push(0);
        }
        payload.push(0);
        auth_message(10, &payload)
    }

    fn ready_sequence() -> Vec<u8> {
        let mut out = auth_message(0, &[]);
        out.extend_from_slice(b"K");
        out.extend_from_slice(&12i32.to_be_bytes());
        out.extend_from_slice(&7i32.to_be_bytes());
        out.extend_from_slice(&9i32.to_be_bytes());
        out.extend_from_slice(b"Z");
        out.extend_from_slice(&5i32.to_be_bytes());
        out.push(b'I');
        out
    }

    fn error_response(message: &str) -> Vec<u8> {
        let mut body = Vec::new();
        for (field, value) in [
            (b'S', "FATAL"),
            (b'V', "FATAL"),
            (b'C', "28000"),
            (b'M', message),
        ] {
            body.push(field);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut out = vec![b'E'];
        out.extend_from_slice(&(4 + body.len() as i32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    async fn read_startup(socket: &mut TcpStream) -> Vec<u8> {
        let len = socket.read_i32().await.unwrap() as usize;
        let mut body = vec![0; len - 4];
        socket.read_exact(&mut body).await.unwrap();
        body
    }

    async fn read_message(socket: &mut TcpStream) -> (u8, Vec<u8>) {
        let tag = socket.read_u8().await.unwrap();
        let len = socket.read_i32().await.unwrap() as usize;
        let mut body = vec![0; len - 4];
        socket.read_exact(&mut body).await.unwrap();
        (tag, body)
    }

    /// Split a SASLInitialResponse body into (mechanism, data).
    fn initial_response(body: &[u8]) -> (String, Vec<u8>) {
        let nul = body.iter().position(|b| *b == 0).unwrap();
        let mechanism = String::from_utf8(body[..nul].to_vec()).unwrap();
        let len = i32::from_be_bytes(body[nul + 1..nul + 5].try_into().unwrap()) as usize;
        (mechanism, body[nul + 5..nul + 5 + len].to_vec())
    }

    /// How the scripted server answers the client's first SASL message.
    enum Script {
        /// Accept exactly this token.
        Accept(&'static str),
        /// Send the discovery document, expect the 0x01 acknowledgement, fail.
        Discovery,
        /// Offer only SCRAM.
        ScramOnly,
    }

    /// Run one scripted connection; returns what the client sent as its
    /// SASL initial response (mechanism, data) and acknowledgement.
    async fn serve(
        listener: TcpListener,
        script: Script,
    ) -> (Option<(String, Vec<u8>)>, Option<Vec<u8>>) {
        let (mut socket, _) = listener.accept().await.unwrap();
        let startup = read_startup(&mut socket).await;
        assert!(
            startup.windows(6).any(|w| w == b"alice\0"),
            "startup names the role"
        );
        if let Script::ScramOnly = script {
            socket
                .write_all(&sasl_offer(&["SCRAM-SHA-256"]))
                .await
                .unwrap();
            return (None, None);
        }
        socket
            .write_all(&sasl_offer(&["OAUTHBEARER"]))
            .await
            .unwrap();
        let (tag, body) = read_message(&mut socket).await;
        assert_eq!(tag, b'p');
        let first = initial_response(&body);
        match script {
            Script::Accept(token) => {
                assert_eq!(
                    first.1,
                    tokio_postgres::oauth::client_initial_response(token)
                );
                socket.write_all(&ready_sequence()).await.unwrap();
                // Keep the socket open until the client goes away.
                let mut sink = Vec::new();
                let _ = socket.read_to_end(&mut sink).await;
                (Some(first), None)
            }
            Script::Discovery => {
                socket
                    .write_all(&auth_message(11, DISCOVERY.as_bytes()))
                    .await
                    .unwrap();
                let (tag, ack) = read_message(&mut socket).await;
                assert_eq!(tag, b'p');
                socket
                    .write_all(&error_response(
                        "OAuth bearer authentication failed for user \"alice\"",
                    ))
                    .await
                    .unwrap();
                (Some(first), Some(ack))
            }
            Script::ScramOnly => unreachable!(),
        }
    }

    async fn connect(
        p: &ConnectionProfile,
        port: u16,
    ) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
        let mut config = config_for(p).unwrap();
        config.ssl_mode(tokio_postgres::config::SslMode::Disable);
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (client, connection) = config.connect_raw(stream, tokio_postgres::NoTls).await?;
        tokio::spawn(connection);
        Ok(client)
    }

    #[tokio::test]
    async fn oauthbearer_signs_in_with_the_token() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(serve(listener, Script::Accept("good-token")));
        let mut p = oauth_profile(PgOAuthFlow::DeviceCode, "");
        p.runtime_cloud_iam_token = Some("good-token".into());
        let client = connect(&p, port).await.expect("signed in");
        drop(client);
        let (first, _) = server.await.unwrap();
        assert_eq!(first.unwrap().0, "OAUTHBEARER");
    }

    #[tokio::test]
    async fn empty_token_reads_the_discovery_document() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(serve(listener, Script::Discovery));
        let mut p = oauth_profile(PgOAuthFlow::DeviceCode, "");
        p.runtime_cloud_iam_token = Some(String::new());
        let Err(error) = connect(&p, port).await else {
            panic!("discovery fails the sign-in");
        };
        let (first, ack) = server.await.unwrap();
        assert_eq!(first.unwrap().1, b"n,,\x01auth=\x01\x01".to_vec());
        assert_eq!(ack.unwrap(), vec![0x01]);

        let Some(OAuthBearerError::Rejected { server_message, .. }) = oauth_error(&error) else {
            panic!("expected a rejected OAuth sign-in, got {error:?}");
        };
        assert!(server_message.as_deref().unwrap().contains("OAuth bearer"));
        let hint = discovery_from_error(&error).unwrap();
        assert_eq!(hint.scope.as_deref(), Some("openid postgres"));
        assert!(
            hint.openid_configuration
                .unwrap()
                .ends_with("openid-configuration")
        );

        let StudioError::AuthenticationMessage(message) =
            connection_error(oauth_error(&error).unwrap())
        else {
            panic!("expected an authentication message");
        };
        assert_eq!(message.key(), Some("services.pg.oauth_token_rejected"));
        assert!(message.fallback().contains("invalid_token"), "{message}");
    }

    #[tokio::test]
    async fn a_server_without_oauthbearer_is_reported_as_not_offering_oauth() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(serve(listener, Script::ScramOnly));
        let mut p = oauth_profile(PgOAuthFlow::DeviceCode, "");
        p.runtime_cloud_iam_token = Some("token".into());
        let Err(error) = connect(&p, port).await else {
            panic!("no OAUTHBEARER");
        };
        server.await.unwrap();
        let Some(OAuthBearerError::NotOffered { mechanisms }) = oauth_error(&error) else {
            panic!("expected NotOffered, got {error:?}");
        };
        assert_eq!(mechanisms, &vec!["SCRAM-SHA-256".to_string()]);
        let StudioError::AuthenticationMessage(message) =
            connection_error(oauth_error(&error).unwrap())
        else {
            panic!("expected an authentication message");
        };
        assert_eq!(message.key(), Some("services.pg.oauth_not_offered"));
        assert!(message.fallback().contains("PostgreSQL 18"));
        assert!(discovery_from_error(&error).is_err());
    }
}
