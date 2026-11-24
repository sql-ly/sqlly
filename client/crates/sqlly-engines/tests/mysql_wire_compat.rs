//! MySQL wire-protocol guards against a scripted in-process server.
//!
//! The server below speaks just enough of the MySQL protocol to stand in for
//! a real one: a v10 handshake offering `caching_sha2_password` with full
//! authentication over plain TCP (RSA key exchange), text-protocol
//! `COM_QUERY` with classic EOF result sets, and the errors a strict or
//! locked-down server returns. It records what the client sent so each test
//! can check the bytes on the wire, not just that a call returned.

use std::sync::{Arc, Mutex};

use rsa::pkcs8::{EncodePublicKey, LineEnding};
use rsa::{Oaep, RsaPrivateKey};
use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_engines::driver::SqlTransport;
use sqlly_engines::mysql::{MySqlTransport, execute_on_client, open_client};
use sqlly_testkit::conformance::run_streaming;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PASSWORD: &str = "s3cret-pass";

const CLIENT_LONG_PASSWORD: u32 = 1;
const CLIENT_CONNECT_WITH_DB: u32 = 1 << 3;
const CLIENT_COMPRESS: u32 = 1 << 5;
const CLIENT_LOCAL_FILES: u32 = 1 << 7;
const CLIENT_PROTOCOL_41: u32 = 1 << 9;
const CLIENT_SSL: u32 = 1 << 11;
const CLIENT_TRANSACTIONS: u32 = 1 << 13;
const CLIENT_SECURE_CONNECTION: u32 = 1 << 15;
const CLIENT_MULTI_STATEMENTS: u32 = 1 << 16;
const CLIENT_MULTI_RESULTS: u32 = 1 << 17;
const CLIENT_PS_MULTI_RESULTS: u32 = 1 << 18;
const CLIENT_PLUGIN_AUTH: u32 = 1 << 19;
const CLIENT_CONNECT_ATTRS: u32 = 1 << 20;
const CLIENT_PLUGIN_AUTH_LENENC: u32 = 1 << 21;
const CLIENT_QUERY_ATTRIBUTES: u32 = 1 << 27;

/// What MySQL 8.0.22 offers: compression yes, query attributes no (they
/// arrived in 8.0.23, and a 8.0.22 server answers a query carrying them with
/// error 1835 "Malformed communication packet").
const SERVER_8_0_22_CAPS: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_COMPRESS
    | CLIENT_LOCAL_FILES
    | CLIENT_PROTOCOL_41
    | CLIENT_SSL
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_MULTI_STATEMENTS
    | CLIENT_MULTI_RESULTS
    | CLIENT_PS_MULTI_RESULTS
    | CLIENT_PLUGIN_AUTH
    | CLIENT_CONNECT_ATTRS
    | CLIENT_PLUGIN_AUTH_LENENC;

#[derive(Clone, Copy)]
struct ServerConfig {
    capabilities: u32,
    /// Refuse every query that names `information_schema`, the way a TiDB
    /// access layer hides `information_schema.tables` from a user.
    hide_information_schema: bool,
}

#[derive(Default, Debug)]
struct Recorded {
    /// Capability flags from each connection's handshake response.
    client_capabilities: Vec<u32>,
    /// Each connection asked for the server's RSA public key (`0x02`).
    public_key_requests: usize,
    /// Each connection's password, recovered by decrypting the RSA blob.
    decrypted_passwords: Vec<String>,
    /// Each connection sent the password in clear text after full-auth.
    cleartext_passwords: usize,
    /// Every `COM_QUERY` text, in order.
    queries: Vec<String>,
    /// `COM_QUERY` payloads the strict 8.0.22 parser rejected as malformed.
    malformed_queries: usize,
}

struct FakeServer {
    port: u16,
    recorded: Arc<Mutex<Recorded>>,
}

impl FakeServer {
    async fn start(config: ServerConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        // 1024-bit keeps key generation quick in debug builds; the client's
        // OAEP padding fits the short password with room to spare.
        let key = Arc::new(RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 1024).unwrap());
        let shared = recorded.clone();
        tokio::spawn(async move {
            let mut connection_id = 1u32;
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let recorded = shared.clone();
                let key = key.clone();
                connection_id += 1;
                tokio::spawn(async move {
                    let _ = serve(socket, connection_id, config, key, recorded).await;
                });
            }
        });
        Self { port, recorded }
    }

    fn profile(&self) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            "fake-mysql",
            "127.0.0.1",
            "app",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: PASSWORD.into(),
            },
        );
        profile.port = self.port;
        profile.database_type = DatabaseType::MySql;
        profile.timeout_seconds = 5;
        // Plain TCP: no TLS, so caching_sha2_password cannot take the
        // "send the password over the secure channel" shortcut.
        profile.encrypt = false;
        profile
    }

    fn recorded(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.recorded.lock().unwrap()
    }
}

struct Wire {
    socket: TcpStream,
    seq: u8,
}

impl Wire {
    async fn read(&mut self) -> std::io::Result<Vec<u8>> {
        let mut header = [0u8; 4];
        self.socket.read_exact(&mut header).await?;
        let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
        self.seq = header[3].wrapping_add(1);
        let mut payload = vec![0u8; len];
        self.socket.read_exact(&mut payload).await?;
        Ok(payload)
    }

    async fn write(&mut self, payload: &[u8]) -> std::io::Result<()> {
        let len = (payload.len() as u32).to_le_bytes();
        let mut packet = vec![len[0], len[1], len[2], self.seq];
        packet.extend_from_slice(payload);
        self.seq = self.seq.wrapping_add(1);
        self.socket.write_all(&packet).await
    }

    async fn ok(&mut self) -> std::io::Result<()> {
        // OK: header, affected rows, last insert id, status (autocommit), warnings.
        self.write(&[0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00])
            .await
    }

    async fn eof(&mut self) -> std::io::Result<()> {
        self.write(&[0xfe, 0x00, 0x00, 0x02, 0x00]).await
    }

    async fn error(&mut self, code: u16, state: &str, message: &str) -> std::io::Result<()> {
        let mut payload = vec![0xff];
        payload.extend_from_slice(&code.to_le_bytes());
        payload.push(b'#');
        payload.extend_from_slice(state.as_bytes());
        payload.extend_from_slice(message.as_bytes());
        self.write(&payload).await
    }

    /// A text-protocol result set with classic EOF markers. `None` is NULL.
    async fn result_set(
        &mut self,
        columns: &[&str],
        rows: &[Vec<Option<String>>],
    ) -> std::io::Result<()> {
        self.write(&[columns.len() as u8]).await?;
        for column in columns {
            let mut def = Vec::new();
            for part in ["def", "", "", "", column, ""] {
                lenenc_str(&mut def, part.as_bytes());
            }
            def.push(0x0c);
            def.extend_from_slice(&33u16.to_le_bytes()); // utf8_general_ci
            def.extend_from_slice(&1024u32.to_le_bytes());
            def.push(0xfd); // VAR_STRING
            def.extend_from_slice(&0u16.to_le_bytes());
            def.push(0x00);
            def.extend_from_slice(&[0, 0]);
            self.write(&def).await?;
        }
        self.eof().await?;
        for row in rows {
            let mut payload = Vec::new();
            for value in row {
                match value {
                    Some(text) => lenenc_str(&mut payload, text.as_bytes()),
                    None => payload.push(0xfb),
                }
            }
            self.write(&payload).await?;
        }
        self.eof().await
    }
}

fn lenenc_str(out: &mut Vec<u8>, bytes: &[u8]) {
    assert!(bytes.len() < 251, "fixture strings stay short");
    out.push(bytes.len() as u8);
    out.extend_from_slice(bytes);
}

async fn serve(
    socket: TcpStream,
    connection_id: u32,
    config: ServerConfig,
    key: Arc<RsaPrivateKey>,
    recorded: Arc<Mutex<Recorded>>,
) -> std::io::Result<()> {
    let mut wire = Wire { socket, seq: 0 };
    let scramble: [u8; 20] = *b"0123456789abcdefghij";

    // Protocol v10 handshake advertising caching_sha2_password.
    let mut hello = vec![10u8];
    hello.extend_from_slice(b"8.0.22\0");
    hello.extend_from_slice(&connection_id.to_le_bytes());
    hello.extend_from_slice(&scramble[..8]);
    hello.push(0);
    hello.extend_from_slice(&(config.capabilities as u16).to_le_bytes());
    hello.push(33);
    hello.extend_from_slice(&0x0002u16.to_le_bytes());
    hello.extend_from_slice(&((config.capabilities >> 16) as u16).to_le_bytes());
    hello.push(21);
    hello.extend_from_slice(&[0u8; 10]);
    hello.extend_from_slice(&scramble[8..]);
    hello.push(0);
    hello.extend_from_slice(b"caching_sha2_password\0");
    wire.write(&hello).await?;

    let response = wire.read().await?;
    let client_caps = u32::from_le_bytes(response[..4].try_into().unwrap());
    recorded
        .lock()
        .unwrap()
        .client_capabilities
        .push(client_caps);

    // No cached credentials: demand full authentication.
    wire.write(&[0x01, 0x04]).await?;
    let reply = wire.read().await?;
    if reply == [0x02] {
        recorded.lock().unwrap().public_key_requests += 1;
        let pem = key
            .to_public_key()
            .to_public_key_pem(LineEnding::LF)
            .unwrap();
        let mut packet = vec![0x01];
        packet.extend_from_slice(pem.as_bytes());
        wire.write(&packet).await?;
        let encrypted = wire.read().await?;
        let Ok(mut xored) = key.decrypt(Oaep::new::<sha1::Sha1>(), &encrypted) else {
            return wire
                .error(1045, "28000", "Access denied (bad RSA payload)")
                .await;
        };
        for (i, byte) in xored.iter_mut().enumerate() {
            *byte ^= scramble[i % scramble.len()];
        }
        let password =
            String::from_utf8_lossy(xored.strip_suffix(&[0]).unwrap_or(&xored)).into_owned();
        let accepted = password == PASSWORD;
        recorded.lock().unwrap().decrypted_passwords.push(password);
        if !accepted {
            return wire
                .error(1045, "28000", "Access denied for user 'app'")
                .await;
        }
    } else {
        // A clear-text password over an unencrypted link: a real server
        // refuses it ("Authentication requires secure connection").
        recorded.lock().unwrap().cleartext_passwords += 1;
        return wire
            .error(2061, "HY000", "Authentication requires secure connection.")
            .await;
    }
    wire.ok().await?;

    loop {
        let Ok(packet) = wire.read().await else {
            return Ok(());
        };
        match packet.first() {
            Some(0x01) => return Ok(()),                 // COM_QUIT
            Some(0x0e) | Some(0x1f) => wire.ok().await?, // COM_PING / COM_RESET_CONNECTION
            Some(0x03) => {
                // Without CLIENT_QUERY_ATTRIBUTES a COM_QUERY is the command
                // byte and the statement text, nothing else. Anything else in
                // front of the text is what 8.0.22 cannot parse.
                let body = &packet[1..];
                let plain = client_caps & CLIENT_QUERY_ATTRIBUTES == 0
                    && body.first().is_some_and(|b| b.is_ascii_alphabetic());
                let Ok(sql) = std::str::from_utf8(body).map(str::to_string) else {
                    recorded.lock().unwrap().malformed_queries += 1;
                    wire.error(1835, "HY000", "Malformed communication packet.")
                        .await?;
                    continue;
                };
                if !plain {
                    recorded.lock().unwrap().malformed_queries += 1;
                    wire.error(1835, "HY000", "Malformed communication packet.")
                        .await?;
                    continue;
                }
                recorded.lock().unwrap().queries.push(sql.clone());
                answer_query(&mut wire, &sql, config).await?;
            }
            _ => wire.error(1047, "08S01", "Unknown command").await?,
        }
    }
}

async fn answer_query(wire: &mut Wire, sql: &str, config: ServerConfig) -> std::io::Result<()> {
    let lower = sql.to_ascii_lowercase();
    if config.hide_information_schema && lower.contains("information_schema") {
        return wire
            .error(
                1142,
                "42000",
                "denied: schema/table information_schema.tables is hidden",
            )
            .await;
    }
    if let Some(vars) = sql.strip_prefix("SELECT @@") {
        // The driver's post-handshake settings probe.
        let names: Vec<String> = vars.split(",@@").map(|v| format!("@@{v}")).collect();
        let values = names
            .iter()
            .map(|name| match name.as_str() {
                "@@max_allowed_packet" => Some("67108864".to_string()),
                "@@wait_timeout" => Some("28800".to_string()),
                _ => None,
            })
            .collect();
        let columns: Vec<&str> = names.iter().map(String::as_str).collect();
        return wire.result_set(&columns, &[values]).await;
    }
    if lower == "show databases" {
        let rows = ["information_schema", "appdb", "sales"]
            .iter()
            .map(|name| vec![Some(name.to_string())])
            .collect::<Vec<_>>();
        return wire.result_set(&["Database"], &rows).await;
    }
    if lower == "select 42 as answer" {
        return wire
            .result_set(&["answer"], &[vec![Some("42".to_string())]])
            .await;
    }
    wire.error(1064, "42000", "You have an error in your SQL syntax")
        .await
}

fn names(databases: &[sqlly_core::DatabaseSummary]) -> Vec<&str> {
    databases.iter().map(|d| d.name.as_str()).collect()
}

// https://github.com/Sequel-Ace/Sequel-Ace/issues/2465
#[tokio::test]
async fn caching_sha2_full_auth_over_plain_tcp_fetches_the_server_public_key() {
    let server = FakeServer::start(ServerConfig {
        capabilities: SERVER_8_0_22_CAPS,
        hide_information_schema: false,
    })
    .await;
    let profile = server.profile();

    let client = open_client(&profile)
        .await
        .expect("caching_sha2_password must sign in over a non-TLS link");
    drop(client);

    let recorded = server.recorded();
    assert_eq!(
        recorded.client_capabilities.len(),
        1,
        "one handshake: {recorded:?}"
    );
    assert_eq!(
        recorded.client_capabilities[0] & CLIENT_SSL,
        0,
        "the link under test is plain TCP"
    );
    assert_eq!(
        recorded.public_key_requests, 1,
        "the client must ask for the server's RSA key itself"
    );
    assert_eq!(recorded.cleartext_passwords, 0);
    assert_eq!(recorded.decrypted_passwords, vec![PASSWORD.to_string()]);
}

// https://github.com/Sequel-Ace/Sequel-Ace/issues/2417
#[tokio::test]
async fn show_databases_on_mysql_8_0_22_sends_a_plain_com_query() {
    let server = FakeServer::start(ServerConfig {
        capabilities: SERVER_8_0_22_CAPS,
        hide_information_schema: false,
    })
    .await;
    let profile = server.profile();

    let databases = MySqlTransport
        .list_databases(&profile)
        .await
        .expect("SHOW DATABASES must work against MySQL 8.0.22");
    assert_eq!(names(&databases), ["information_schema", "appdb", "sales"]);

    let recorded = server.recorded();
    assert_eq!(recorded.malformed_queries, 0, "{recorded:?}");
    assert!(
        recorded.queries.iter().any(|q| q == "SHOW DATABASES"),
        "{:?}",
        recorded.queries
    );
    for caps in &recorded.client_capabilities {
        assert_eq!(caps & CLIENT_QUERY_ATTRIBUTES, 0, "no query attributes");
        assert_eq!(
            caps & CLIENT_COMPRESS,
            0,
            "no compressed framing, though the server offers it"
        );
    }
}

// https://github.com/beekeeper-studio/beekeeper-studio/issues/4994
#[tokio::test]
async fn hidden_information_schema_does_not_break_connect_or_queries() {
    let server = FakeServer::start(ServerConfig {
        capabilities: SERVER_8_0_22_CAPS,
        hide_information_schema: true,
    })
    .await;
    let profile = server.profile();

    // "Test connection" and "Connect": both open a session.
    MySqlTransport
        .test_connection(&profile)
        .await
        .expect("connect must not read information_schema");
    // The server node's children.
    let databases = MySqlTransport
        .list_databases(&profile)
        .await
        .expect("database list must not read information_schema");
    assert_eq!(names(&databases), ["information_schema", "appdb", "sales"]);

    // A query tab on a fresh session keeps working.
    let client = open_client(&profile).await.expect("open");
    let (result, outcome) = run_streaming(|bus| {
        let client = &client;
        let profile = &profile;
        async move { execute_on_client(client, profile, "SELECT 42 AS answer", &bus, None).await }
    })
    .await;
    result.expect("query tab runs");
    assert!(outcome.completed);
    assert_eq!(
        outcome.result_sets[0].1,
        vec![vec![CellValue::String("42".into())]]
    );

    let recorded = server.recorded();
    assert!(
        recorded
            .queries
            .iter()
            .all(|q| !q.to_ascii_lowercase().contains("information_schema")),
        "connect path touched information_schema: {:?}",
        recorded.queries
    );
}
