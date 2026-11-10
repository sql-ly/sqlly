//! `sqlly://` deep links, SQLite/DuckDB file opens, and second-instance hand-off.

use std::path::Path;
use std::sync::Mutex;

use sqlly_core::{
    AuthenticationType, ConnectionProfile, DatabaseType, DeepLink, SecretProvider, SecretRef,
    describe_deep_link, file_connection_name, file_database_engine, parse_deep_link,
};
use sqlly_services::open_requests;

use super::*;
use crate::connection::ConnectionStore;
use crate::deep_link_sheet::{DeepLinkSheet, DeepLinkSheetEvent, DeepLinkSheetModel};

static PENDING_OPEN_URLS: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub(crate) fn queue_open_url(url: String) {
    if let Ok(mut pending) = PENDING_OPEN_URLS.lock() {
        pending.push(url);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn prepare_startup_arg(arg: &str) -> String {
    if arg.starts_with("sqlly://")
        || arg.starts_with("file://")
        || sqlly_core::connect_url::is_raw_connection_uri(arg)
    {
        return arg.to_string();
    }
    let path = Path::new(arg);
    if file_database_engine(path).is_some()
        && path.is_relative()
        && let Ok(cwd) = std::env::current_dir()
    {
        return cwd.join(path).to_string_lossy().to_string();
    }
    arg.to_string()
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn deep_link_candidate(arg: &str) -> bool {
    arg.starts_with("sqlly://")
        || arg.starts_with("file://")
        || sqlly_core::connect_url::is_raw_connection_uri(arg)
        || file_database_engine(Path::new(arg)).is_some()
}

/// What a second instance writes into the open-request hand-off for the
/// running one: a raw connection URL loses its password first (the running
/// instance asks for it), so the password never touches disk.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn hand_off_form(raw: &str) -> String {
    if sqlly_core::connect_url::is_raw_connection_uri(raw) {
        sqlly_core::connect_url::redact_connection_uri(raw)
    } else {
        raw.to_string()
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn drain_startup_deep_links() -> Vec<String> {
    STARTUP_DEEP_LINKS
        .lock()
        .ok()
        .and_then(|mut links| links.take())
        .unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) static STARTUP_DEEP_LINKS: Mutex<Option<Vec<String>>> = Mutex::new(None);

impl AppView {
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn poll_open_requests(&mut self, cx: &mut Context<Self>) {
        let _ = open_requests::touch_instance_marker();
        let pending_urls = PENDING_OPEN_URLS
            .lock()
            .ok()
            .map(|mut urls| std::mem::take(&mut *urls))
            .unwrap_or_default();
        for raw in pending_urls {
            self.handle_deep_link_raw(raw, cx);
        }
        let Ok(requests) = open_requests::take_requests(chrono::Utc::now()) else {
            return;
        };
        for request in requests {
            if let Ok(link) = parse_deep_link(&request.raw) {
                tracing::debug!(action = %describe_deep_link(&link), "open request received");
            }
            self.handle_deep_link_raw(request.raw, cx);
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn poll_open_requests(&mut self, _cx: &mut Context<Self>) {}

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_startup_deep_links(&mut self, cx: &mut Context<Self>) {
        for raw in drain_startup_deep_links() {
            self.handle_deep_link_raw(raw, cx);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_deep_link_raw(&mut self, raw: String, cx: &mut Context<Self>) {
        let link = match parse_deep_link(&raw) {
            Ok(link) => link,
            Err(error) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.deep_links.open_failed",
                        &[("error", error)],
                    ),
                );
                cx.notify();
                return;
            }
        };
        tracing::debug!(action = %describe_deep_link(&link), "deep link offered");
        let connection_found = match &link {
            DeepLink::Open { connection, .. } | DeepLink::Query { connection, .. } => {
                self.find_connection_by_name(connection).is_some()
            }
            DeepLink::OpenDatabaseFile { path, engine } => {
                self.find_file_profile(path, *engine).is_some()
            }
            DeepLink::NewConnection { .. } => true,
            DeepLink::ConnectUrl { parsed } => {
                let profiles = ConnectionStore::global().list();
                let matched = sqlly_core::connect_url::find_matching_profile(parsed, &profiles)
                    .map(|profile| profile.name.clone());
                let model = DeepLinkSheetModel::new_connect_url(link.clone(), matched);
                self.show_deep_link_sheet(model, cx);
                return;
            }
        };
        let mut model = DeepLinkSheetModel::new(link, connection_found);
        if let DeepLink::OpenDatabaseFile { path, engine } = &model.link
            && !path.exists()
        {
            model.error = Some(format!(
                "{} file not found: {}",
                engine.display_name(),
                path.to_string_lossy()
            ));
        }
        self.show_deep_link_sheet(model, cx);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn show_deep_link_sheet(&mut self, model: DeepLinkSheetModel, cx: &mut Context<Self>) {
        let sheet = cx.new(|cx| DeepLinkSheet::new(model, cx));
        cx.subscribe(&sheet, |this, _sheet, event, cx| match event {
            DeepLinkSheetEvent::Cancelled => {
                this.overlays.deep_link_sheet = None;
                cx.notify();
            }
            DeepLinkSheetEvent::Confirmed(link) => {
                this.overlays.deep_link_sheet = None;
                this.confirm_deep_link((**link).clone(), cx);
            }
        })
        .detach();
        self.overlays.deep_link_sheet = Some(sheet);
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn find_connection_by_name(&self, name: &str) -> Option<ConnectionProfile> {
        ConnectionStore::global()
            .list()
            .into_iter()
            .find(|profile| profile.name.eq_ignore_ascii_case(name))
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn find_file_profile(&self, path: &Path, engine: DatabaseType) -> Option<ConnectionProfile> {
        let canonical = path.canonicalize().ok();
        ConnectionStore::global()
            .list()
            .into_iter()
            .find(|profile| {
                profile.database_type == engine
                    && (profile.server == path.to_string_lossy()
                        || canonical
                            .as_ref()
                            .is_some_and(|resolved| profile.server == resolved.to_string_lossy()))
            })
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn confirm_deep_link(&mut self, link: DeepLink, cx: &mut Context<Self>) {
        match link {
            DeepLink::Open {
                connection,
                database,
                table,
            } => {
                let Some(mut profile) = self.find_connection_by_name(&connection) else {
                    return;
                };
                if let Some(database) = database {
                    profile.database = Some(database);
                }
                self.connect_profile_then(profile.clone(), cx, move |app, cx| {
                    if let Some(table) = table {
                        let (schema, name) = split_table_ref(&table);
                        app.sidebar.update(cx, |sidebar, cx| {
                            sidebar.reveal_object(Some(profile.id.0), schema.as_deref(), name, cx);
                        });
                    }
                });
            }
            DeepLink::Query {
                connection,
                database,
                sql,
            } => {
                let Some(mut profile) = self.find_connection_by_name(&connection) else {
                    return;
                };
                if let Some(database) = database {
                    profile.database = Some(database);
                }
                let needs_connect = self.active_profile.as_ref().map(|p| p.id) != Some(profile.id);
                let run = move |app: &mut Self, cx: &mut Context<Self>| {
                    app.new_tab(cx, Some(sql));
                    app.set_status(
                        StatusLevel::Info,
                        crate::localization::text("shell.deep_links.query_opened"),
                    );
                    cx.notify();
                };
                if needs_connect {
                    self.connect_profile_then(profile, cx, run);
                } else {
                    run(self, cx);
                }
            }
            DeepLink::NewConnection { parsed } => {
                let app_view = cx.entity();
                cx.defer(move |app| {
                    let Some(win) = app.active_window() else {
                        return;
                    };
                    let _ = win.update(app, |_root, window, app| {
                        app_view.update(app, |this, cx| {
                            this.open_new_connection_editor(window, cx);
                            if let Some(editor) = this.overlays.connection_editor.clone() {
                                editor.update(cx, |editor, cx| {
                                    editor.prefill_from_parsed(*parsed, window, cx);
                                });
                            }
                        });
                    });
                });
            }
            DeepLink::ConnectUrl { parsed } => self.connect_from_url(*parsed, cx),
            DeepLink::OpenDatabaseFile { path, engine } => {
                let profile = if let Some(existing) = self.find_file_profile(&path, engine) {
                    existing
                } else {
                    let mut profile = ConnectionStore::create_sql_login(
                        file_connection_name(&path, engine),
                        path.to_string_lossy(),
                        0,
                        None,
                        String::new(),
                        String::new(),
                        false,
                        false,
                    );
                    profile.database_type = engine;
                    profile.authentication = AuthenticationType::SqlLogin {
                        username: String::new(),
                        secret_ref: SecretRef {
                            provider: SecretProvider::InMemory,
                            key: String::new(),
                        },
                    };
                    ConnectionStore::global().add(profile.clone());
                    self.sidebar
                        .update(cx, |sidebar, cx| sidebar.refresh_connections(cx));
                    profile
                };
                self.connect_profile(profile, cx);
            }
        }
        cx.notify();
    }
}

impl AppView {
    /// Gridex #18: connect to the saved connection a raw connection URL
    /// names (taking the URL's database), or else to a session-only
    /// connection built from it. The URL's password is used only in memory.
    #[cfg(not(target_arch = "wasm32"))]
    fn connect_from_url(
        &mut self,
        parsed: sqlly_core::ParsedConnectionUrl,
        cx: &mut Context<Self>,
    ) {
        let store = ConnectionStore::global();
        let profiles = store.list();
        if let Some(saved) = sqlly_core::connect_url::find_matching_profile(&parsed, &profiles) {
            let mut profile = saved.clone();
            if let Some(database) = parsed.database.clone().filter(|d| !d.is_empty()) {
                profile.database = Some(database);
            }
            self.connect_profile(profile, cx);
            return;
        }
        match sqlly_core::connect_url::unsaved_profile(&parsed) {
            Ok(profile) => {
                store.add_unsaved(profile.clone());
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.refresh_connections(cx));
                self.connect_profile(profile, cx);
            }
            Err(error) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.deep_links.connect_url_failed",
                        &[("error", &error)],
                    ),
                );
            }
        }
    }
}

fn split_table_ref(table: &str) -> (Option<String>, &str) {
    if let Some((schema, name)) = table.split_once('.') {
        (Some(schema.to_string()), name)
    } else {
        (None, table)
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// Gridex #18: a raw connection URL on the command line is a startup
    /// link, passed through untouched (a database named `app.db` is not a
    /// relative file path), and loses its password before the hand-off to a
    /// running instance.
    #[test]
    fn raw_connection_urls_are_startup_links() {
        let url = "mysql://user:s3cret@server:3306/app.db";
        assert!(deep_link_candidate(url));
        assert_eq!(prepare_startup_arg(url), url);
        assert_eq!(hand_off_form(url), "mysql://user@server:3306/app.db");
        assert!(deep_link_candidate("postgres://h/db"));
        assert!(!deep_link_candidate("--verbose"));
        assert!(!deep_link_candidate("https://example.com"));
        assert_eq!(
            hand_off_form("sqlly://open?connection=prod"),
            "sqlly://open?connection=prod"
        );
    }
}
