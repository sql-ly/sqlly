#![cfg(not(target_arch = "wasm32"))]
//! pgAdmin, Azure Data Studio / VS Code, pg_service.conf and DbGate imports,
//! and the export side for every format SQLLY writes for other tools.

use std::collections::HashSet;

use sqlly_core::connection::TlsMode;
use sqlly_core::connection_url::parse_connection_url;
use sqlly_core::profile_store::{
    ExportedProfile, ExportedSshAuth, ExportedSshHop, ExportedSshTunnel,
};
use sqlly_core::{
    ConnectionEnvironment, DatabaseType, ImportedConnection, ToolExportFormat, ToolFileKind,
    ToolImportPreview, detect_kind, export_for_tool, parse_tool_file, preview_to_bundle,
};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/tool_import/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
    .replace("\r\n", "\n")
}

fn conn<'a>(preview: &'a ToolImportPreview, name: &str) -> &'a ImportedConnection {
    preview
        .connections
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no connection {name} in {:?}", preview.connections))
}

#[test]
fn pgadmin_servers_json_fixture() {
    let text = fixture("servers.json");
    assert_eq!(
        detect_kind("servers.json", &text),
        Some(ToolFileKind::PgAdminServersJson)
    );
    let preview = parse_tool_file(ToolFileKind::PgAdminServersJson, &text).unwrap();
    assert_eq!(preview.connections.len(), 2);

    let orders = conn(&preview, "Orders (prod)");
    assert_eq!(orders.parsed.engine, DatabaseType::Postgres);
    assert_eq!(orders.parsed.server, "pg1.acme.internal");
    assert_eq!(orders.parsed.port, Some(5433));
    assert_eq!(orders.parsed.database.as_deref(), Some("orders"));
    assert_eq!(orders.parsed.username.as_deref(), Some("orders_ro"));
    assert_eq!(orders.parsed.tls_mode, Some(TlsMode::VerifyFull));
    assert_eq!(
        orders.parsed.tls_ca_file.as_deref(),
        Some(std::path::Path::new("/etc/ssl/acme-ca.pem"))
    );
    assert_eq!(orders.group.as_deref(), Some("Acme"));
    assert!(orders.had_password, "a PassFile counts as a saved password");

    let legacy = conn(&preview, "Legacy reporting");
    assert_eq!(legacy.parsed.tls_mode, Some(TlsMode::Require));
    assert_eq!(
        legacy.group, None,
        "pgAdmin's default Servers group is no folder"
    );
    let tunnel = legacy.ssh_tunnel.as_ref().expect("ssh tunnel");
    assert_eq!(tunnel.target.host, "bastion.acme.internal");
    assert_eq!(tunnel.target.port, 2222);
    assert_eq!(tunnel.target.username, "jump");
    assert_eq!(
        tunnel.target.auth,
        ExportedSshAuth::KeyFile {
            path: "/home/me/.ssh/id_ed25519".into()
        }
    );

    assert_eq!(preview.skipped.len(), 1);
    assert_eq!(preview.skipped[0].name, "By service only");
}

#[test]
fn azure_data_studio_settings_fixture() {
    let text = fixture("settings.json");
    assert_eq!(
        detect_kind("settings.json", &text),
        Some(ToolFileKind::AzureDataStudioSettingsJson)
    );
    let preview = parse_tool_file(ToolFileKind::AzureDataStudioSettingsJson, &text).unwrap();
    let names: Vec<&str> = preview
        .connections
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "Sales DW",
            r"WORKSTATION\SQLEXPRESS",
            "Analytics PG",
            "Local dev"
        ],
        "the VS Code copy of Sales DW is the same server and is imported once"
    );

    let sales = conn(&preview, "Sales DW");
    assert_eq!(sales.parsed.engine, DatabaseType::SqlServer);
    assert_eq!(sales.parsed.server, "sql01.acme.internal");
    assert_eq!(sales.parsed.port, Some(14330));
    assert_eq!(sales.parsed.username.as_deref(), Some("sales_reader"));
    assert_eq!(sales.parsed.encrypt, Some(true));
    assert_eq!(sales.parsed.trust_server_certificate, Some(true));
    assert_eq!(sales.group.as_deref(), Some("Acme"));
    assert!(sales.had_password);

    let express = conn(&preview, r"WORKSTATION\SQLEXPRESS");
    assert!(express.parsed.integrated_security);
    assert_eq!(express.parsed.port, None);
    assert_eq!(express.group, None, "the ROOT group is no folder");

    let pg = conn(&preview, "Analytics PG");
    assert_eq!(pg.parsed.engine, DatabaseType::Postgres);
    assert_eq!(pg.parsed.database.as_deref(), Some("analytics"));
    assert_eq!(pg.parsed.tls_mode, Some(TlsMode::Require));

    assert_eq!(conn(&preview, "Local dev").parsed.encrypt, Some(false));
    assert_eq!(preview.skipped.len(), 1);
    assert_eq!(preview.skipped[0].name, "Kusto cluster");
}

#[test]
fn pg_service_conf_fixture() {
    let text = fixture("pg_service.conf");
    assert_eq!(
        detect_kind("pg_service.conf", &text),
        Some(ToolFileKind::PgServiceConf)
    );
    assert_eq!(
        detect_kind(".pg_service.conf", &text),
        Some(ToolFileKind::PgServiceConf)
    );
    let preview = parse_tool_file(ToolFileKind::PgServiceConf, &text).unwrap();
    assert_eq!(preview.connections.len(), 2);
    let orders = conn(&preview, "orders");
    assert_eq!(
        orders.parsed.server, "pg1.acme.internal",
        "first of a host list"
    );
    assert_eq!(orders.parsed.port, Some(5433));
    assert_eq!(orders.parsed.username.as_deref(), Some("orders_ro"));
    assert_eq!(orders.parsed.tls_mode, Some(TlsMode::VerifyCa));
    assert!(!orders.had_password);
    let local = conn(&preview, "local");
    assert_eq!(local.parsed.server, "localhost");
    assert_eq!(local.parsed.database.as_deref(), Some("scratch"));
    assert!(local.had_password);
}

#[test]
fn dbgate_env_fixture() {
    let text = fixture("dbgate.env");
    assert_eq!(
        detect_kind("dbgate.env", &text),
        Some(ToolFileKind::DbGateEnv)
    );
    assert_eq!(detect_kind(".env", &text), Some(ToolFileKind::DbGateEnv));
    let preview = parse_tool_file(ToolFileKind::DbGateEnv, &text).unwrap();
    assert_eq!(preview.connections.len(), 3);

    let pg = conn(&preview, "Orders PG");
    assert_eq!(pg.parsed.engine, DatabaseType::Postgres);
    assert_eq!(pg.parsed.port, Some(5433));
    assert_eq!(pg.parsed.database.as_deref(), Some("orders"));
    assert_eq!(pg.parsed.username.as_deref(), Some("orders_ro"));
    assert_eq!(pg.parsed.tls_mode, Some(TlsMode::VerifyFull));
    assert!(pg.read_only);
    assert!(pg.had_password);
    assert_eq!(
        pg.ssh_tunnel.as_ref().unwrap().target.auth,
        ExportedSshAuth::KeyFile {
            path: "/home/me/.ssh/id_ed25519".into()
        }
    );

    let ops = conn(&preview, "Ops SQL");
    assert_eq!(ops.parsed.engine, DatabaseType::SqlServer);
    assert!(ops.parsed.integrated_security);
    assert_eq!(ops.parsed.trust_server_certificate, Some(true));

    let lite = conn(&preview, "Local file");
    assert_eq!(lite.parsed.engine, DatabaseType::Sqlite);
    assert_eq!(lite.parsed.server, "/data/app.sqlite");

    let skipped: Vec<&str> = preview.skipped.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(skipped, ["Mongo", "Missing host"]);
}

#[test]
fn existing_dbeaver_detection_is_unaffected() {
    let text = fixture("data-sources.json");
    assert_eq!(
        detect_kind("data-sources.json", &text),
        Some(ToolFileKind::DbeaverDataSourcesJson)
    );
}

// ---------------------------------------------------------------------------
// Export

const SECRET: &str = "s3cret-never-exported";

fn profile(
    name: &str,
    url: &str,
    group: Option<&str>,
    edit: impl FnOnce(&mut ExportedProfile),
) -> ExportedProfile {
    let parsed = parse_connection_url(url).unwrap();
    let preview = ToolImportPreview {
        kind: None,
        connections: vec![ImportedConnection {
            name: name.to_string(),
            parsed,
            group: group.map(str::to_string),
            environment: ConnectionEnvironment::Unset,
            read_only: false,
            had_password: true,
            has_ssh_handler: false,
            ssh_tunnel: None,
        }],
        skipped: Vec::new(),
    };
    let (bundle, _) = preview_to_bundle(&preview, &HashSet::new());
    let mut profile = bundle.profiles.into_iter().next().unwrap();
    edit(&mut profile);
    profile
}

fn sample_profiles() -> Vec<ExportedProfile> {
    vec![
        profile(
            "Orders",
            &format!(
                "postgres://orders_ro:{SECRET}@pg1.acme.internal:5433/orders?sslmode=verify-full"
            ),
            Some("Acme"),
            |p| {
                p.environment = ConnectionEnvironment::Production;
                p.block_all_but_select = true;
                p.ssh_tunnel = Some(ExportedSshTunnel {
                    target: ExportedSshHop {
                        host: "bastion.acme.internal".into(),
                        port: 2222,
                        username: "jump".into(),
                        auth: ExportedSshAuth::KeyFile {
                            path: "/home/me/.ssh/id_ed25519".into(),
                        },
                    },
                    jump: None,
                    keepalive: Default::default(),
                });
            },
        ),
        profile(
            "Sales DW",
            &format!("sqlserver://sales_reader:{SECRET}@sql01.acme.internal:14330/SalesDW"),
            Some("Acme"),
            |_| {},
        ),
        profile(
            "Shop",
            &format!("mysql://shop:{SECRET}@mysql.acme.internal:3306/shop"),
            None,
            |_| {},
        ),
        profile("Local file", "sqlite:///data/app.sqlite", None, |_| {}),
        profile("Cache", "redis://cache.acme.internal:6379/0", None, |_| {}),
    ]
}

fn kind_for(format: ToolExportFormat) -> ToolFileKind {
    match format {
        ToolExportFormat::PgAdminServersJson => ToolFileKind::PgAdminServersJson,
        ToolExportFormat::AzureDataStudioSettingsJson => ToolFileKind::AzureDataStudioSettingsJson,
        ToolExportFormat::PgServiceConf => ToolFileKind::PgServiceConf,
        ToolExportFormat::DbeaverDataSourcesJson => ToolFileKind::DbeaverDataSourcesJson,
        ToolExportFormat::JetBrainsDataSourcesXml => ToolFileKind::JetBrainsDataSourcesXml,
        ToolExportFormat::DbGateEnv => ToolFileKind::DbGateEnv,
    }
}

/// Which details each target format can hold, so the round trip checks
/// exactly what the format promises.
struct Carries {
    group: bool,
    user: bool,
}

fn carries(format: ToolExportFormat) -> Carries {
    match format {
        ToolExportFormat::PgAdminServersJson
        | ToolExportFormat::AzureDataStudioSettingsJson
        | ToolExportFormat::DbeaverDataSourcesJson => Carries {
            group: true,
            user: true,
        },
        ToolExportFormat::JetBrainsDataSourcesXml => Carries {
            group: true,
            user: false,
        },
        ToolExportFormat::PgServiceConf | ToolExportFormat::DbGateEnv => Carries {
            group: false,
            user: true,
        },
    }
}

#[test]
fn every_export_round_trips_through_its_importer() {
    let profiles = sample_profiles();
    for format in ToolExportFormat::ALL {
        let export = export_for_tool(format, &profiles);
        assert!(
            !export.contents.contains(SECRET),
            "{format:?} wrote a password"
        );
        let expected: Vec<&ExportedProfile> = profiles
            .iter()
            .filter(|p| format.supports(p.database_type))
            .collect();
        assert_eq!(export.exported, expected.len(), "{format:?}");
        assert_eq!(
            export.skipped.len(),
            profiles.len() - expected.len(),
            "{format:?}"
        );
        assert!(!expected.is_empty(), "{format:?} exports nothing");

        let kind = kind_for(format);
        assert_eq!(
            detect_kind(format.file_name(), &export.contents),
            Some(kind),
            "{format:?} output is not detected by its own importer:\n{}",
            export.contents
        );
        let preview = parse_tool_file(kind, &export.contents).unwrap();
        assert!(
            preview.skipped.is_empty(),
            "{format:?} skipped {:?}",
            preview.skipped
        );
        assert_eq!(preview.connections.len(), expected.len(), "{format:?}");
        let what = carries(format);
        for original in expected {
            let back = conn(&preview, &original.name);
            let label = format!("{format:?} {}", original.name);
            assert_eq!(
                back.parsed.engine.connection_kind(),
                original.database_type.connection_kind(),
                "{label}"
            );
            assert_eq!(back.parsed.server, original.server, "{label}");
            if original.port != 0 {
                assert_eq!(
                    back.parsed.port.unwrap_or(original.port),
                    original.port,
                    "{label}"
                );
            }
            assert_eq!(back.parsed.database, original.database, "{label}");
            if what.user {
                let user =
                    match &original.authentication {
                        sqlly_core::profile_store::ExportedAuthentication::SqlLogin {
                            username,
                        } if !username.is_empty() => Some(username.as_str()),
                        _ => None,
                    };
                assert_eq!(back.parsed.username.as_deref(), user, "{label}");
            }
            if what.group {
                assert_eq!(back.group, original.project, "{label}");
            }
        }
    }
}

#[test]
fn tunnels_tls_and_read_only_survive_where_the_format_holds_them() {
    let profiles = sample_profiles();
    for format in [
        ToolExportFormat::PgAdminServersJson,
        ToolExportFormat::DbeaverDataSourcesJson,
        ToolExportFormat::DbGateEnv,
    ] {
        let export = export_for_tool(format, &profiles);
        let preview = parse_tool_file(kind_for(format), &export.contents).unwrap();
        let orders = conn(&preview, "Orders");
        let hop = &orders.ssh_tunnel.as_ref().expect("tunnel").target;
        assert_eq!(hop.host, "bastion.acme.internal", "{format:?}");
        assert_eq!(hop.port, 2222, "{format:?}");
        assert_eq!(hop.username, "jump", "{format:?}");
        assert_eq!(
            hop.auth,
            ExportedSshAuth::KeyFile {
                path: "/home/me/.ssh/id_ed25519".into()
            },
            "{format:?}"
        );
    }
    for format in [
        ToolExportFormat::PgAdminServersJson,
        ToolExportFormat::PgServiceConf,
        ToolExportFormat::DbGateEnv,
    ] {
        let export = export_for_tool(format, &profiles);
        let preview = parse_tool_file(kind_for(format), &export.contents).unwrap();
        assert_eq!(
            conn(&preview, "Orders").parsed.tls_mode,
            Some(TlsMode::VerifyFull),
            "{format:?}"
        );
    }
    for format in [
        ToolExportFormat::DbeaverDataSourcesJson,
        ToolExportFormat::DbGateEnv,
    ] {
        let export = export_for_tool(format, &profiles);
        let preview = parse_tool_file(kind_for(format), &export.contents).unwrap();
        assert!(conn(&preview, "Orders").read_only, "{format:?}");
    }
    let dbeaver = export_for_tool(ToolExportFormat::DbeaverDataSourcesJson, &profiles);
    let preview = parse_tool_file(ToolFileKind::DbeaverDataSourcesJson, &dbeaver.contents).unwrap();
    assert_eq!(
        conn(&preview, "Orders").environment,
        ConnectionEnvironment::Production
    );
}

#[test]
fn exports_are_deterministic_and_skip_unsupported_engines_by_name() {
    let profiles = sample_profiles();
    let mut reversed = profiles.clone();
    reversed.reverse();
    for format in ToolExportFormat::ALL {
        assert_eq!(
            export_for_tool(format, &profiles),
            export_for_tool(format, &reversed),
            "{format:?}"
        );
    }
    let pgadmin = export_for_tool(ToolExportFormat::PgAdminServersJson, &profiles);
    let skipped: Vec<&str> = pgadmin.skipped.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(skipped, ["Cache", "Local file", "Sales DW", "Shop"]);
    assert_eq!(
        pgadmin.skipped[0].reason.key(),
        Some("services.tool_export.skip_reasons.engine_unsupported")
    );
}

#[test]
fn pgadmin_export_writes_every_required_attribute() {
    let export = export_for_tool(ToolExportFormat::PgAdminServersJson, &sample_profiles());
    let value: serde_json::Value = serde_json::from_str(&export.contents).unwrap();
    let server = &value["Servers"]["1"];
    for key in [
        "Name",
        "Group",
        "Port",
        "Username",
        "SSLMode",
        "MaintenanceDB",
        "Host",
    ] {
        assert!(server.get(key).is_some(), "missing {key}");
    }
    assert_eq!(server["ConnectionParameters"]["sslmode"], "verify-full");
    assert_eq!(server["SSLMode"], "verify-full");
}

#[test]
fn azure_data_studio_export_writes_sql_server_port_the_sql_server_way() {
    let export = export_for_tool(
        ToolExportFormat::AzureDataStudioSettingsJson,
        &sample_profiles(),
    );
    let value: serde_json::Value = serde_json::from_str(&export.contents).unwrap();
    let sales = value["datasource.connections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["providerName"] == "MSSQL")
        .unwrap();
    assert_eq!(sales["options"]["server"], "sql01.acme.internal,14330");
    assert_eq!(sales["options"]["savePassword"], false);
    assert_eq!(
        value["mssql.connections"][0]["server"],
        "sql01.acme.internal,14330"
    );
}
