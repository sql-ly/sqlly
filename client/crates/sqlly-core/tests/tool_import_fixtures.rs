#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashSet;

use sqlly_core::{
    ConnectionEnvironment, DatabaseType, ToolFileKind, dedupe_key, detect_kind,
    parse_tableplus_groups_plist, parse_tableplus_plist_with_groups, parse_tool_file,
    parse_tool_file_bytes, preview_to_bundle,
};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/tool_import/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

#[test]
fn jetbrains_xml_fixture() {
    let text = fixture("dataSources.xml");
    assert_eq!(
        detect_kind("dataSources.xml", &text),
        Some(ToolFileKind::JetBrainsDataSourcesXml)
    );
    let preview = parse_tool_file(ToolFileKind::JetBrainsDataSourcesXml, &text).unwrap();
    assert_eq!(preview.connections.len(), 4);
    assert_eq!(preview.skipped.len(), 1);
    assert!(
        preview
            .skipped
            .iter()
            .any(|s| s.name == "oracle-skip" && s.reason.to_ascii_lowercase().contains("oracle"))
    );

    let pg = preview
        .connections
        .iter()
        .find(|c| c.name == "pg-local")
        .unwrap();
    assert_eq!(pg.parsed.engine, DatabaseType::Postgres);
    assert_eq!(pg.parsed.port, Some(5432));
    assert_eq!(pg.parsed.database.as_deref(), Some("appdb"));
    assert_eq!(pg.parsed.username.as_deref(), Some("app"));
    assert_eq!(pg.parsed.encrypt, Some(true));

    let mssql = preview
        .connections
        .iter()
        .find(|c| c.name == "mssql-named")
        .unwrap();
    assert_eq!(mssql.parsed.server, r"db01\SQL2022");
    assert_eq!(mssql.parsed.port, Some(1433));
    assert_eq!(mssql.parsed.database.as_deref(), Some("Sales"));
    assert_eq!(mssql.parsed.encrypt, Some(true));
    assert_eq!(mssql.parsed.trust_server_certificate, Some(true));
    assert_eq!(mssql.group.as_deref(), Some("Acme/Prod"));

    let mysql = preview
        .connections
        .iter()
        .find(|c| c.name == "mysql-v6")
        .unwrap();
    assert_eq!(mysql.parsed.server, "::1");
    assert_eq!(mysql.parsed.port, Some(3307));
    assert_eq!(mysql.parsed.username.as_deref(), Some("web"));
    assert!(mysql.parsed.password.is_none());
    assert!(mysql.had_password);

    let sqlite = preview
        .connections
        .iter()
        .find(|c| c.name == "sqlite-file")
        .unwrap();
    assert_eq!(sqlite.parsed.server, "/Users/me/data/app.sqlite");
    assert_eq!(sqlite.parsed.port, None);
}

#[test]
fn dbeaver_json_fixture() {
    let text = fixture("data-sources.json");
    assert_eq!(
        detect_kind("data-sources.json", &text),
        Some(ToolFileKind::DbeaverDataSourcesJson)
    );
    let preview = parse_tool_file(ToolFileKind::DbeaverDataSourcesJson, &text).unwrap();
    assert_eq!(preview.connections.len(), 4);
    assert_eq!(preview.skipped.len(), 1);

    let pg = preview
        .connections
        .iter()
        .find(|c| c.name == "pg-prod")
        .unwrap();
    assert_eq!(pg.environment, ConnectionEnvironment::Production);
    assert!(pg.read_only);
    assert!(pg.has_ssh_handler);
    assert!(pg.had_password);
    assert!(pg.parsed.password.is_none());
    assert!(pg.ssh_tunnel.is_some());

    let mysql = preview
        .connections
        .iter()
        .find(|c| c.name == "mysql-url")
        .unwrap();
    assert_eq!(mysql.parsed.server, "2001:db8::1");
    assert_eq!(mysql.environment, ConnectionEnvironment::Dev);

    assert!(
        preview
            .skipped
            .iter()
            .any(|s| s.name == "ora" && s.reason.contains("unsupported"))
    );

    let (bundle, skipped) = preview_to_bundle(&preview, &HashSet::new());
    assert_eq!(bundle.profiles.len(), 3);
    assert!(
        skipped
            .iter()
            .any(|s| s.name == "pg-prod-copy" && s.reason.contains("already exists"))
    );
}

#[test]
fn preview_to_bundle_is_secret_free_and_dedupes() {
    let text = fixture("data-sources.json");
    let preview = parse_tool_file(ToolFileKind::DbeaverDataSourcesJson, &text).unwrap();
    let pg = preview
        .connections
        .iter()
        .find(|c| c.name == "pg-prod")
        .unwrap();
    let key = dedupe_key(
        pg.parsed.engine,
        &pg.parsed.server,
        pg.parsed.port.unwrap_or(5432),
        pg.parsed.database.as_deref(),
    );
    let existing = HashSet::from([key]);
    let (bundle, skipped) = preview_to_bundle(&preview, &existing);
    assert_eq!(bundle.profiles.len(), 2);
    assert!(
        skipped
            .iter()
            .any(|s| s.name == "pg-prod" && s.reason.contains("already exists"))
    );
    let json = serde_json::to_string(&bundle).unwrap();
    assert!(!json.contains("\"password\""));
    assert!(!json.contains("dropped-on-import"));
}

#[test]
fn navicat_ncx_fixture() {
    let text = fixture("connections.ncx");
    assert_eq!(
        detect_kind("connections.ncx", &text),
        Some(ToolFileKind::NavicatNcx)
    );
    let preview = parse_tool_file(ToolFileKind::NavicatNcx, &text).unwrap();
    assert_eq!(preview.connections.len(), 4);
    assert_eq!(preview.skipped.len(), 1);
    assert!(
        preview
            .skipped
            .iter()
            .any(|s| s.name == "ora-skip" && s.reason.to_ascii_lowercase().contains("oracle"))
    );

    let pg = preview
        .connections
        .iter()
        .find(|c| c.name == "pg-prod")
        .unwrap();
    assert_eq!(pg.parsed.engine, DatabaseType::Postgres);
    assert_eq!(pg.parsed.port, Some(5433));
    assert_eq!(pg.parsed.database.as_deref(), Some("appdb"));
    assert_eq!(pg.parsed.username.as_deref(), Some("app"));
    assert_eq!(pg.parsed.encrypt, Some(true));
    assert_eq!(pg.parsed.trust_server_certificate, Some(true));
    assert!(pg.has_ssh_handler);
    let tunnel = pg.ssh_tunnel.as_ref().unwrap();
    assert_eq!(tunnel.target.port, 2222);
    assert_eq!(
        tunnel.target.auth,
        sqlly_core::profile_store::ExportedSshAuth::KeyFile {
            path: "/Users/me/.ssh/id_ed25519".into()
        }
    );
    assert_eq!(pg.group.as_deref(), Some("Acme/Prod"));

    let mysql = preview
        .connections
        .iter()
        .find(|c| c.name == "mysql-local")
        .unwrap();
    assert!(mysql.had_password);
    assert!(mysql.parsed.password.is_none());

    let mssql = preview
        .connections
        .iter()
        .find(|c| c.name == "mssql-win")
        .unwrap();
    assert!(mssql.parsed.integrated_security);
    assert_eq!(mssql.parsed.server, r"db01\SQL2022");

    let sqlite = preview
        .connections
        .iter()
        .find(|c| c.name == "sqlite-file")
        .unwrap();
    assert_eq!(sqlite.parsed.server, "/Users/me/data/app.sqlite");
    assert_eq!(sqlite.parsed.port, None);

    let (bundle, _) = preview_to_bundle(&preview, &HashSet::new());
    assert_eq!(bundle.profiles.len(), 4);
    let json = serde_json::to_string(&bundle).unwrap();
    assert!(!json.contains("E00641F75A8FFC2E7AF00CC29732DB6E"));
}

#[test]
fn tableplus_plist_fixture() {
    let bytes = std::fs::read(format!(
        "{}/tests/fixtures/tool_import/Connections.plist",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert_eq!(
        detect_kind("Connections.plist", &text),
        Some(ToolFileKind::TablePlusConnectionsPlist)
    );
    let groups_bytes = std::fs::read(format!(
        "{}/tests/fixtures/tool_import/ConnectionGroups.plist",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let groups = parse_tableplus_groups_plist(&groups_bytes).unwrap();
    let preview = parse_tableplus_plist_with_groups(&bytes, &groups).unwrap();
    assert_eq!(preview.connections.len(), 3);
    assert_eq!(preview.skipped.len(), 1);
    assert!(
        preview
            .skipped
            .iter()
            .any(|s| s.name == "redis-skip" && s.reason.contains("Redis"))
    );

    let pg = preview
        .connections
        .iter()
        .find(|c| c.name == "pg-local")
        .unwrap();
    assert_eq!(pg.group.as_deref(), Some("Acme"));
    assert_eq!(pg.environment, ConnectionEnvironment::Local);
    assert_eq!(pg.parsed.encrypt, Some(true));
    assert_eq!(pg.parsed.trust_server_certificate, Some(false));

    let mysql = preview
        .connections
        .iter()
        .find(|c| c.name == "mysql-ssh")
        .unwrap();
    assert_eq!(mysql.environment, ConnectionEnvironment::Production);
    assert!(mysql.has_ssh_handler);
    let tunnel = mysql.ssh_tunnel.as_ref().unwrap();
    assert_eq!(tunnel.target.host, "bastion.acme.internal");
    assert!(matches!(
        tunnel.target.auth,
        sqlly_core::profile_store::ExportedSshAuth::KeyFile { .. }
    ));
}

#[test]
fn tableplus_json_fixture() {
    let text = fixture("export.tableplusconnection");
    assert_eq!(
        detect_kind("export.tableplusconnection", &text),
        Some(ToolFileKind::TablePlusConnectionJson)
    );
    let preview = parse_tool_file(ToolFileKind::TablePlusConnectionJson, &text).unwrap();
    assert_eq!(preview.connections.len(), 3);
    assert_eq!(preview.skipped.len(), 1);

    let pg = preview
        .connections
        .iter()
        .find(|c| c.name == "pg-local")
        .unwrap();
    assert_eq!(pg.environment, ConnectionEnvironment::Local);
    assert_eq!(pg.parsed.encrypt, Some(true));
    assert_eq!(pg.parsed.trust_server_certificate, Some(false));

    let mysql = preview
        .connections
        .iter()
        .find(|c| c.name == "mysql-ssh")
        .unwrap();
    assert_eq!(mysql.environment, ConnectionEnvironment::Production);
    assert_eq!(
        mysql.ssh_tunnel.as_ref().unwrap().target.host,
        "bastion.acme.internal"
    );
}

#[test]
fn tableplus_plist_binary_round_trip() {
    let bytes = std::fs::read(format!(
        "{}/tests/fixtures/tool_import/Connections.plist",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let value = plist::Value::from_reader(std::io::Cursor::new(&bytes)).unwrap();
    let mut binary = Vec::new();
    value.to_writer_binary(&mut binary).unwrap();
    let preview = parse_tool_file_bytes(ToolFileKind::TablePlusConnectionsPlist, &binary).unwrap();
    assert_eq!(preview.connections.len(), 3);
    assert_eq!(preview.skipped.len(), 1);
}
