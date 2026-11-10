use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use gpui::{
    AnyElement, Context, Entity, EventEmitter, ExternalPaths, Hsla, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, ScrollHandle, Styled,
    Subscription, Window, anchored, deferred, div, img, prelude::*, px, relative,
};
use sqlly_core::{
    AuthenticationType, AzureSqlDiscovery, ConnectionEnvironment, ConnectionManagementTree,
    ConnectionOptionsForm, ConnectionOptionsFormMode, ConnectionProfile, ConnectionTestStatus,
    DatabaseType, EntraAccountBinding, RegisteredServer, RegisteredServerStore, SecretProvider,
    SecretRef, TlsMode,
};
use sqlly_services::security::azure::EntraAccountProfile;
use uuid::Uuid;

use crate::azure_registry::EntraAccountRegistryStore;
use crate::connection::{
    ClientEntity, ConnectionStore, EnvironmentEntity, ProjectEntity, SafetyFeatures, SqlAuthAccount,
};
use crate::dialog::{Dialog, DialogFrame};
use crate::entity_management::{EntityKind, SafetyField};
use crate::host_validator::{HostValidationResult, validate_host};
use crate::scrollbars::DraggableScrollbar;
use crate::theme::Theme;
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::select::{SearchableVec, Select, SelectEvent, SelectState};

mod accounts;
mod actions;
mod azure;
mod cloud_iam_form;
mod entities;
mod form;
mod init;
mod tree;
mod widgets;

/// Which TLS path row a Browse… button fills.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) enum TlsPathField {
    Ca,
    ClientCert,
    ClientKey,
}

use accounts::*;
#[cfg(test)]
use entities::*;
#[cfg(test)]
use tree::*;
#[cfg(test)]
use widgets::*;

/// Opening size of the dialog, in design units (the frame scales them). The
/// same numbers size the popped-out window so detaching does not reflow the
/// form.
pub(super) const DIALOG_WIDTH: f32 = 1170.0;
pub(super) const DIALOG_HEIGHT: f32 = 580.0;

const BASE_DISCLOSURE_ICON_SIZE: f32 = 10.0;
const DISCLOSURE_ICON_SCALE: f32 = 1.25;
const DISCLOSURE_ICON_SIZE: f32 = BASE_DISCLOSURE_ICON_SIZE * DISCLOSURE_ICON_SCALE;
const TREE_LEVEL_INDENT: f32 = 7.0;
/// Sentinel item in the client / project selects that clears the assignment.
fn none_choice() -> String {
    crate::localization::text("connections.management.no_entity_choice")
}

/// Top-level section of the connection workspace, mirroring the five tabs of
/// Swift's `ConnectionWorkspaceController` (Connections | Accounts | Clients |
/// Projects | Environments), rendered as a System Settings-style sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspacePane {
    Connections,
    Accounts,
    Clients,
    Projects,
    Environments,
}

impl WorkspacePane {
    pub const ALL: [WorkspacePane; 5] = [
        WorkspacePane::Connections,
        WorkspacePane::Accounts,
        WorkspacePane::Clients,
        WorkspacePane::Projects,
        WorkspacePane::Environments,
    ];

    fn label(self) -> String {
        crate::localization::text(match self {
            WorkspacePane::Connections => "connections.management.connections_pane",
            WorkspacePane::Accounts => "connections.management.accounts_pane",
            WorkspacePane::Clients => "connections.management.clients_pane",
            WorkspacePane::Projects => "connections.management.projects_pane",
            WorkspacePane::Environments => "connections.management.environments_pane",
        })
    }

    /// Glyph shown inside the sidebar item's colored icon square. Restricted
    /// to widely-covered BMP geometric shapes so every platform font renders
    /// them.
    fn glyph(self) -> &'static str {
        match self {
            WorkspacePane::Connections => "\u{2261}", // ≡
            WorkspacePane::Accounts => "@",
            WorkspacePane::Clients => "\u{25C6}",      // ◆
            WorkspacePane::Projects => "\u{25A3}",     // ▣
            WorkspacePane::Environments => "\u{25CF}", // ●
        }
    }

    /// Tint of the sidebar item's icon square (System Settings-style colored
    /// icons), sourced from theme colors so it adapts to light/dark.
    fn tint(self) -> Hsla {
        match self {
            WorkspacePane::Connections => Theme::accent(),
            WorkspacePane::Accounts => Theme::kind_database(),
            WorkspacePane::Clients => Theme::kind_table(),
            WorkspacePane::Projects => Theme::kind_procedure(),
            WorkspacePane::Environments => Theme::success(),
        }
    }

    /// The entity collection this pane edits, when it is an entity pane.
    fn entity_kind(self) -> Option<EntityKind> {
        match self {
            WorkspacePane::Clients => Some(EntityKind::Client),
            WorkspacePane::Projects => Some(EntityKind::Project),
            WorkspacePane::Environments => Some(EntityKind::Environment),
            _ => None,
        }
    }
}

/// Authentication mode selectable in new/edit connection forms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    SqlLogin,
    Integrated,
    EntraBrowser,
    AwsRdsIam,
    GcpCloudSqlIam,
    /// libSQL / Turso bearer token (blank for an anonymous local server).
    AuthToken,
}

impl AuthMode {
    pub const ALL: [AuthMode; 6] = [
        AuthMode::SqlLogin,
        AuthMode::Integrated,
        AuthMode::EntraBrowser,
        AuthMode::AwsRdsIam,
        AuthMode::GcpCloudSqlIam,
        AuthMode::AuthToken,
    ];

    pub fn label(self) -> String {
        crate::localization::text(match self {
            AuthMode::SqlLogin => "connections.auth.sql_login",
            AuthMode::Integrated => "connections.auth.windows",
            AuthMode::EntraBrowser => "connections.auth.microsoft_entra",
            AuthMode::AwsRdsIam => "connections.auth.aws_iam",
            AuthMode::GcpCloudSqlIam => "connections.auth.google_iam",
            AuthMode::AuthToken => "connections.auth.token",
        })
    }

    /// The mode a freshly chosen engine starts in.
    pub fn default_for(engine: DatabaseType) -> AuthMode {
        if crate::engine_form::uses_auth_token(engine) {
            AuthMode::AuthToken
        } else {
            AuthMode::SqlLogin
        }
    }

    pub fn supported_for(self, engine: DatabaseType) -> bool {
        match self {
            AuthMode::SqlLogin => {
                !crate::engine_form::is_file_engine(engine)
                    && !crate::engine_form::uses_auth_token(engine)
            }
            AuthMode::AuthToken => crate::engine_form::uses_auth_token(engine),
            AuthMode::Integrated => engine == DatabaseType::SqlServer,
            // Redis: Azure Cache for Redis / Azure Managed Redis.
            AuthMode::EntraBrowser => matches!(
                engine,
                DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase | DatabaseType::Redis
            ),
            AuthMode::AwsRdsIam => {
                matches!(
                    crate::capabilities::cloud_iam_auth(engine),
                    crate::capabilities::Capability::Available
                )
            }
            // Google IAM is Cloud SQL only; AWS IAM also covers ElastiCache /
            // MemoryDB (Redis protocol), so the two modes gate separately.
            AuthMode::GcpCloudSqlIam => {
                !matches!(
                    engine.connection_kind(),
                    sqlly_lang::sqlmodel::ConnectionKind::Resp
                ) && matches!(
                    crate::capabilities::cloud_iam_auth(engine),
                    crate::capabilities::Capability::Available
                )
            }
        }
    }
}

/// An unsaved duplicate the management form is editing: where it came from
/// and the copy (with its re-minted secret references) it was built from.
pub(crate) struct PendingDuplicate {
    /// Id of the saved profile the copy was made from; the copy is inserted
    /// directly after it on save.
    pub(crate) source: Uuid,
    pub(crate) copy: sqlly_core::DuplicatedProfile,
}

pub enum ConnectionManagementEvent {
    /// A profile was saved (created or updated). Carries the saved profile.
    Saved(ConnectionProfile),
    /// The user asked to connect using this profile (it was also saved).
    ConnectRequested(ConnectionProfile),
    /// A saved profile was deleted.
    Deleted(Uuid),
    /// The user dismissed the management window.
    Cancelled,
    /// The dialog moved into its own OS window. The host should drop its
    /// inline overlay slot without treating this as a dismissal — the same
    /// entity keeps running, rendered by the detached window.
    PoppedOut,
    /// User clicked a discovered Azure database — link it as a saved
    /// connection and connect.
    LinkAzureDatabase(AzureSqlDiscovery),
    /// The Accounts pane's "Add Account…" button was clicked. The host runs
    /// the interactive Entra browser sign-in and threads the outcome back via
    /// [`ConnectionManagement::on_signin_success`] /
    /// [`ConnectionManagement::on_signin_error`].
    AddEntraAccountRequested,
    RefreshAzureServersRequested,
    /// An entity (client / project / environment) was added, saved, or
    /// removed — the host should refresh the sidebar grouping display.
    EntitiesChanged,
    /// Saving a new connection hit the free tier's saved-connection limit;
    /// the host opens the unlock window.
    UnlockRequested,
}

impl EventEmitter<ConnectionManagementEvent> for ConnectionManagement {}

/// A flat, owned snapshot of a tree node used for rendering. Decoupling the
/// render plan from the borrowed `ConnectionManagementTree` lets the recursive
/// walk run under an immutable borrow while individual rows are built under the
/// mutable borrow `cx.listener` needs.
#[derive(Clone)]
struct RowPlan {
    id: String,
    label: String,
    depth: usize,
    has_children: bool,
    badges: Vec<String>,
}

struct EntityPaneRow {
    id: Uuid,
    name: String,
    color_hex: Option<String>,
    image_path: Option<String>,
    safety: SafetyFeatures,
}

pub struct ConnectionManagement {
    /// Route-aware service resolution shared with the app (see
    /// `ServiceRouter`): the inline form carries a saved ViaRelay route
    /// forward, so its Test Connection / database probe must resolve through
    /// that route instead of the in-process (direct-dialing) services.
    router: crate::service_router::ServiceRouter,
    log: crate::log_console::DiagnosticLog,
    /// Active workspace section (System Settings-style sidebar selection).
    pane: WorkspacePane,
    /// True when the connection form has edits that have not been saved.
    dirty: bool,
    /// Set when a close was requested with unsaved edits: the footer turns
    /// into the Save / Discard / Keep-editing prompt until it is answered.
    close_confirm: bool,
    /// True once the dialog has been popped out into its own OS window, where
    /// the window boundary is the dialog boundary and closing means closing
    /// the window rather than clearing the host's overlay slot.
    windowed: bool,
    /// Live host validation for the Server field (Swift parity).
    host_validation: HostValidationResult,
    /// Entra account binding saved into the profile for Entra auth modes.
    entra_account: Option<EntraAccountBinding>,
    /// Registered Entra accounts (Accounts pane + auth binding picker).
    entra_registry: EntraAccountRegistryStore,
    /// Shared SQL-token minter, injected by the app. Lets Test Connection and
    /// the Databases dropdown re-mint an expired Entra token before dialing
    /// (they used to dial with whatever token was last stamped and fail with
    /// "token expired"). `None` in tests.
    entra_sql_token_provider: Option<crate::azure_sql_token::EntraSqlTokenProvider>,
    entra_accounts: Vec<EntraAccountProfile>,
    entra_selected: Option<usize>,
    entra_status: String,
    entra_in_flight: bool,
    sql_accounts: Vec<SqlAuthAccount>,
    sql_selected: Option<Uuid>,
    connection_sql_account: Option<Uuid>,
    sql_account_name: String,
    sql_account_username: String,
    sql_account_name_input: Entity<InputState>,
    sql_account_username_input: Entity<InputState>,
    sql_account_password: Entity<InputState>,
    sql_account_status: String,
    /// Lazy database dropdown on the Database field.
    database_dropdown_open: bool,
    database_list: Option<Vec<String>>,
    database_loading: bool,
    database_list_error: Option<String>,
    /// Entity pane editor state (Clients / Projects / Environments).
    entity_selected: Option<Uuid>,
    entity_name: String,
    entity_color: String,
    entity_image_path: String,
    entity_name_input: Entity<InputState>,
    entity_color_picker: Entity<ColorPickerState>,
    entity_safety: SafetyFeatures,
    entity_status: String,
    search: String,
    name: String,
    server: String,
    port: String,
    database_type: DatabaseType,
    /// The Redis-family "Auto-detect" picker entry is chosen.
    engine_auto_detect: bool,
    database: String,
    username: String,
    aws_region: String,
    aws_profile: String,
    #[cfg(not(target_arch = "wasm32"))]
    gcp_adc_status: String,
    password: Entity<InputState>,
    client: String,
    project: String,
    timeout: String,
    statement_timeout: String,
    startup_sql: String,
    /// Free-form notes kept on the connection (blank = none).
    notes: String,
    search_input: Entity<InputState>,
    name_input: Entity<InputState>,
    server_input: Entity<InputState>,
    port_input: Entity<InputState>,
    engine_select: Entity<SelectState<SearchableVec<String>>>,
    database_input: Entity<InputState>,
    username_input: Entity<InputState>,
    aws_region_input: Entity<InputState>,
    #[cfg(not(target_arch = "wasm32"))]
    aws_profile_select: Entity<SelectState<Vec<String>>>,
    #[cfg(not(target_arch = "wasm32"))]
    aws_profile_options: Vec<String>,
    #[cfg(not(target_arch = "wasm32"))]
    aws_profiles_loaded: bool,
    #[cfg(not(target_arch = "wasm32"))]
    aws_profile_select_stale: bool,
    timeout_input: Entity<InputState>,
    statement_timeout_input: Entity<InputState>,
    startup_sql_input: Entity<TextareaState>,
    notes_input: Entity<TextareaState>,
    url_paste_input: Entity<InputState>,
    url_paste_open: bool,
    client_select: Entity<SelectState<Vec<String>>>,
    project_select: Entity<SelectState<Vec<String>>>,
    _subscriptions: Vec<Subscription>,
    environment: ConnectionEnvironment,
    auth_mode: AuthMode,
    encrypt: bool,
    trust: bool,
    /// TLS verification mode for PostgreSQL/MySQL engines. Always a resolved
    /// mode (never `Legacy`); `encrypt`/`trust` are kept in sync so older
    /// readers of a saved profile behave the same.
    tls_mode: TlsMode,
    tls_ca_file: String,
    tls_client_cert_file: String,
    tls_client_key_file: String,
    tls_ca_input: Entity<InputState>,
    tls_cert_input: Entity<InputState>,
    tls_key_input: Entity<InputState>,
    tls_mode_select: Entity<SelectState<Vec<String>>>,
    block_select: bool,
    rollback: bool,
    editing_id: Option<Uuid>,
    /// Set while the form holds an unsaved copy of a saved connection (the
    /// list footer's duplicate button). Save inserts the copy beside its
    /// source and copies the source's keyring secrets into the copy's own
    /// entries; anything else that resets the form drops it unwritten.
    /// Never set together with `editing_id`.
    duplicate_of: Option<PendingDuplicate>,
    expanded: HashSet<String>,
    selected: Option<String>,
    tree: ConnectionManagementTree,
    azure_discoveries: Vec<AzureSqlDiscovery>,
    azure_servers_loading: bool,
    azure_servers_error: Option<String>,
    azure_server_dropdown_open: bool,
    /// True once the "ask my admin for the Reader role" message has been copied
    /// to the clipboard from the info affordance, so the UI can confirm it.
    azure_server_help_copied: bool,
    testing: bool,
    /// A Test All pass is running; the button disables so passes never
    /// overlap and connections are only ever dialed one at a time.
    testing_all: bool,
    test_result: Option<String>,
    test_success: bool,
    status_message: String,
    /// Unified dialog chrome geometry (position/size + in-flight drags).
    frame: DialogFrame,
    /// Width of the left pane (server list) in scaled pixels.
    split_left_width: f32,
    connection_tree_scroll: ScrollHandle,
    account_list_scroll: ScrollHandle,
    entity_list_scroll: ScrollHandle,
    /// True while the user is dragging the split divider.
    is_split_resizing: bool,
    split_resize_start_pos: Point<Pixels>,
    split_resize_start_width: f32,
}

impl Render for ConnectionManagement {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = crate::localization::text(if self.dirty {
            "connections.management.dialog_title_edited"
        } else {
            "connections.management.dialog_title"
        });

        let content: AnyElement = match self.pane {
            WorkspacePane::Connections => self.render_connections_pane(window, cx),
            WorkspacePane::Accounts => self.render_accounts_pane(cx),
            pane => {
                let kind = pane.entity_kind().expect("non-entity panes handled above");
                self.render_entity_pane(kind, cx)
            }
        };
        let nav = self.render_nav(cx);

        // ----- Dialog -----
        // The chrome owns the outer move/resize; the extra overlay listeners
        // keep the body-internal split-divider drag tracked across the whole
        // window, same as the old backdrop did.
        //
        // This is a data-entry surface holding half-typed servers, ports, and
        // credentials, so it deliberately does NOT quick-close: Escape and a
        // click on the scrim do nothing, and every close route runs through
        // `request_close`, which prompts when there are unsaved edits.
        let mut dialog = Dialog::new("cm-dialog", |this: &mut Self| &mut this.frame)
            .title(title)
            .easy_close(false)
            .bare_body()
            .use_default_close(false)
            .dismiss_on_scrim_click(false)
            .windowed(self.windowed)
            .on_close(|this, window, cx| this.request_close(window, cx))
            .children_body(crate::browser_preview::preview_banner(
                &if crate::connection::connections_are_proxied() {
                    crate::localization::text("connections.management.proxy_browser_banner")
                } else {
                    crate::localization::text("connections.management.browser_preview_banner")
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(nav)
                    .child(content),
            );

        if !self.windowed {
            dialog = dialog.detachable(|this, _window, cx| this.pop_out(cx));
        }

        if self.close_confirm {
            dialog = Self::unsaved_prompt(dialog, cx);
        }

        dialog
            .build(self, window, cx)
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            // Keeps a scrollbar drag tracking once the pointer leaves the
            // narrow track (the list/tree scrollbars in this dialog).
            .scrollbar_drag_area()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sqlly_core::ConnectionManagementNodeKind;
    use uuid::Uuid;

    fn temp_store() -> ConnectionStore {
        let path = std::env::temp_dir().join(format!("sqlly-cm-test-{}.json", Uuid::new_v4()));
        ConnectionStore::from_path(path)
    }

    #[test]
    fn copy_duplicate_secrets_copies_readable_entries_and_counts_the_rest() {
        keyring::mock::install();
        let readable_from = Uuid::new_v4();
        let readable_to = Uuid::new_v4();
        let missing_from = Uuid::new_v4();
        let missing_to = Uuid::new_v4();
        crate::sql_credentials::store_password(readable_from, "hunter2").unwrap();
        let keychain = |id: Uuid| sqlly_core::SecretRef {
            provider: sqlly_core::SecretProvider::MacOsKeychain,
            key: id.to_string(),
        };
        let secrets = vec![
            sqlly_core::SecretCopy {
                from: keychain(readable_from),
                to: keychain(readable_to),
            },
            sqlly_core::SecretCopy {
                from: keychain(missing_from),
                to: keychain(missing_to),
            },
        ];

        let failures = actions::copy_duplicate_secrets(&secrets);

        assert_eq!(failures, 1, "only the entry with nothing stored fails");
        assert_eq!(
            crate::sql_credentials::load_password(readable_to)
                .unwrap()
                .as_deref(),
            Some("hunter2"),
            "the copy owns its own entry with the same value"
        );
        assert_eq!(
            crate::sql_credentials::load_password(readable_from)
                .unwrap()
                .as_deref(),
            Some("hunter2"),
            "the original's entry is untouched"
        );
        assert_eq!(
            crate::sql_credentials::load_password(missing_to).unwrap(),
            None
        );
    }

    #[test]
    fn secrets_not_copied_warning_reads_naturally() {
        assert_eq!(actions::secrets_not_copied_warning(0), None);
        let one = actions::secrets_not_copied_warning(1).unwrap();
        assert!(
            one.starts_with("one saved secret could not be read"),
            "{one}"
        );
        let many = actions::secrets_not_copied_warning(3).unwrap();
        assert!(
            many.starts_with("3 saved secrets could not be read"),
            "{many}"
        );
    }

    #[test]
    fn carry_unexposed_fields_keeps_everything_the_inline_form_has_no_control_for() {
        let mut source =
            ConnectionStore::create_sql_login("src", "db", 5432, None, "u", "", true, false);
        source.route = sqlly_core::connection::ConnectionRoute::ViaRelay {
            relay_device_id: "dev".into(),
            pair_id: "pair".into(),
            relay_hint: None,
        };
        source.confirm_data_changes = true;
        source.allow_unbounded_mutations = false;
        source.production_override = Some(true);
        source.rollback_policy_defaulted = true;
        source.provenance = sqlly_core::ConnectionProvenance::Imported;
        source.status.record_test_success(Utc::now());
        source.favorite = true;
        source.application_name = "custom-app".into();
        source.tunnel_profile = Some(Uuid::new_v4());
        source.transport = sqlly_core::TransportChain {
            proxy: Some(sqlly_core::ProxyHop {
                kind: sqlly_core::ProxyKind::Socks5,
                host: "proxy".into(),
                port: 1080,
                username: None,
                password: None,
            }),
            ssh: None,
            kube: None,
            ssm: None,
        };
        source.masking.include.push(sqlly_core::MaskRule {
            pattern: "ssn".into(),
            scope: sqlly_core::RuleScope::ColumnName,
        });
        source.hide_schema("app", "staging");
        source.display_time_zone = Some(sqlly_core::DisplayTimeZone::Named("Asia/Tokyo".into()));

        // What the form rebuilt from its own fields: everything else default.
        let mut rebuilt = ConnectionStore::create_sql_login(
            "renamed",
            "db2",
            5433,
            Some("x".into()),
            "u2",
            "",
            false,
            true,
        );
        form::carry_unexposed_fields(&mut rebuilt, &source);

        // Form-owned fields are untouched…
        assert_eq!(rebuilt.name, "renamed");
        assert_eq!(rebuilt.server, "db2");
        assert_eq!(rebuilt.port, 5433);
        assert_ne!(rebuilt.id, source.id);
        // …and every unexposed one now matches the source.
        assert_eq!(rebuilt.route, source.route);
        assert_eq!(rebuilt.confirm_data_changes, source.confirm_data_changes);
        assert_eq!(
            rebuilt.allow_unbounded_mutations,
            source.allow_unbounded_mutations
        );
        assert_eq!(rebuilt.production_override, source.production_override);
        assert_eq!(
            rebuilt.rollback_policy_defaulted,
            source.rollback_policy_defaulted
        );
        assert_eq!(rebuilt.provenance, source.provenance);
        assert_eq!(rebuilt.status, source.status);
        assert_eq!(rebuilt.favorite, source.favorite);
        assert_eq!(rebuilt.application_name, source.application_name);
        assert_eq!(rebuilt.tunnel_profile, source.tunnel_profile);
        assert_eq!(rebuilt.transport, source.transport);
        assert_eq!(rebuilt.masking, source.masking);
        assert_eq!(rebuilt.hidden_schemas, source.hidden_schemas);
        assert_eq!(rebuilt.display_time_zone, source.display_time_zone);
    }

    #[test]
    fn build_tree_from_empty_store_has_root_only() {
        let store = temp_store();
        let tree = ConnectionManagement::build_tree_from(&store);
        assert!(
            tree.nodes
                .iter()
                .any(|n| n.kind == ConnectionManagementNodeKind::SqlServersRoot)
        );
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|n| n.kind == ConnectionManagementNodeKind::SavedProfile)
                .count(),
            0
        );
    }

    #[test]
    fn new_connection_auth_modes_exclude_entra_device_code() {
        assert_eq!(
            AuthMode::ALL.map(AuthMode::label),
            [
                "SQL Login",
                "Windows",
                "Microsoft Entra",
                "AWS IAM",
                "Google IAM",
                "Auth token"
            ]
        );
    }

    /// AWS IAM covers RDS/Aurora and ElastiCache / MemoryDB (Redis); Google
    /// IAM is Cloud SQL only, so it is not offered on Redis connections.
    #[test]
    fn aws_iam_is_offered_for_redis_but_google_iam_is_not() {
        assert!(AuthMode::AwsRdsIam.supported_for(DatabaseType::Redis));
        assert!(AuthMode::AwsRdsIam.supported_for(DatabaseType::AmazonElastiCache));
        assert!(AuthMode::AwsRdsIam.supported_for(DatabaseType::AmazonMemoryDb));
        assert!(!AuthMode::GcpCloudSqlIam.supported_for(DatabaseType::Redis));
        assert!(!AuthMode::GcpCloudSqlIam.supported_for(DatabaseType::AmazonMemoryDb));
        assert!(AuthMode::GcpCloudSqlIam.supported_for(DatabaseType::Postgres));
    }

    /// libSQL / Turso is the only engine that signs in with a token, and it
    /// starts in that mode; every other engine keeps SQL Login.
    #[test]
    fn auth_token_mode_is_libsql_only_and_its_default() {
        assert!(AuthMode::AuthToken.supported_for(DatabaseType::LibSql));
        assert!(!AuthMode::SqlLogin.supported_for(DatabaseType::LibSql));
        assert!(!AuthMode::Integrated.supported_for(DatabaseType::LibSql));
        assert!(!AuthMode::AwsRdsIam.supported_for(DatabaseType::LibSql));
        for engine in [
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::ClickHouse,
            DatabaseType::Sqlite,
        ] {
            assert!(!AuthMode::AuthToken.supported_for(engine), "{engine:?}");
        }
        assert_eq!(
            AuthMode::default_for(DatabaseType::LibSql),
            AuthMode::AuthToken
        );
        assert_eq!(
            AuthMode::default_for(DatabaseType::Postgres),
            AuthMode::SqlLogin
        );
    }

    #[test]
    fn sole_entra_account_is_automatically_bindable() {
        let account = EntraAccountProfile::new(
            "alex@contoso.com",
            "tenant",
            "home",
            Some("Alex".to_string()),
            Utc::now(),
        );
        let binding = sole_entra_binding(std::slice::from_ref(&account)).unwrap();
        assert_eq!(binding.account_id, account.id.0);
        assert_eq!(binding.tenant_id, "tenant");
        assert_eq!(binding.username.as_deref(), Some("alex@contoso.com"));
        assert!(sole_entra_binding(&[]).is_none());
        assert!(sole_entra_binding(&[account.clone(), account]).is_none());
    }

    #[test]
    fn connection_tree_disclosure_icon_is_125_percent_of_the_original() {
        assert_eq!(DISCLOSURE_ICON_SCALE, 1.25);
        assert_eq!(DISCLOSURE_ICON_SIZE, 12.5);
    }

    #[test]
    fn connection_tree_level_indent_is_reduced_by_half() {
        assert_eq!(TREE_LEVEL_INDENT, 14.0 * 0.5);
    }

    #[test]
    fn connection_tree_content_width_grows_for_deeper_longer_rows() {
        let short = RowPlan {
            id: "short".to_string(),
            label: "SQL".to_string(),
            depth: 0,
            has_children: false,
            badges: Vec::new(),
        };
        let long = RowPlan {
            id: "long".to_string(),
            label: "a-very-long-server-name.database.windows.net".to_string(),
            depth: 4,
            has_children: false,
            badges: vec!["linked".to_string()],
        };
        assert!(connection_tree_content_width(&[long]) > connection_tree_content_width(&[short]));
    }

    #[test]
    fn saved_connection_health_is_badged_and_described() {
        let store = temp_store();
        let mut profile = ConnectionStore::create_sql_login(
            "primary",
            "localhost",
            1433,
            None,
            "sa",
            "secret",
            true,
            false,
        );
        let observed = Utc::now() - chrono::Duration::minutes(5);
        profile
            .status
            .record_failure(observed, "password=hunter2 refused");
        store.add(profile.clone());

        let tree = ConnectionManagement::build_tree_from(&store);
        let node = tree
            .nodes
            .iter()
            .find(|node| node.id.0 == format!("profile:{}", profile.id.0))
            .unwrap();
        assert!(node.badges.iter().any(|badge| badge == "failed"));
        let description =
            connection_health_description(&profile, observed + chrono::Duration::minutes(5));
        assert!(description.contains("failed · 5m ago"));
        assert!(description.contains("password=<redacted> refused"));
    }

    #[test]
    fn connection_health_age_is_bounded_for_clock_skew() {
        assert_eq!(
            relative_health_age(chrono::Duration::seconds(-5)),
            "just now"
        );
        assert_eq!(relative_health_age(chrono::Duration::hours(2)), "2h ago");
        assert_eq!(relative_health_age(chrono::Duration::days(3)), "3d ago");
    }

    #[test]
    fn locked_scrollbar_uses_a_dim_full_track_thumb() {
        assert_eq!(scrollbar_metrics(300.0, 0.0, 0.0), (false, 0.0, 1.0));
    }

    #[test]
    fn scrollable_scrollbar_thumb_tracks_offset() {
        let (scrollable, start, size) = scrollbar_metrics(100.0, 300.0, -150.0);
        assert!(scrollable);
        assert_eq!(size, 0.25);
        assert_eq!(start, 0.375);
    }

    #[test]
    fn entity_icon_picker_accepts_supported_extensions_case_insensitively() {
        for path in [
            "client.PNG",
            "project.jpg",
            "environment.JPEG",
            "icon.gif",
            "icon.svg",
            "icon.ico",
            "icon.bmp",
            "icon.webp",
        ] {
            assert!(is_supported_icon_path(Path::new(path)), "{path}");
        }
        assert!(!is_supported_icon_path(Path::new("icon.txt")));
        assert!(!is_supported_icon_path(Path::new("icon")));
    }

    #[test]
    fn build_tree_from_store_surfaces_saved_profiles() {
        let store = temp_store();
        let profile = ConnectionStore::create_sql_login(
            "MyConn",
            "localhost",
            1433,
            Some("app".to_string()),
            "sa",
            "pw",
            true,
            false,
        );
        store.add(profile);

        let tree = ConnectionManagement::build_tree_from(&store);
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|n| n.kind == ConnectionManagementNodeKind::SavedProfile)
                .count(),
            1
        );
        assert!(tree.nodes.iter().any(|n| n.label == "MyConn"));
        // The registered server id must match the profile id so a tree
        // selection resolves back to the right profile.
        let saved = tree
            .nodes
            .iter()
            .find(|n| n.kind == ConnectionManagementNodeKind::SavedProfile)
            .unwrap();
        assert!(saved.profile_id.is_some());
    }

    #[test]
    fn build_tree_from_store_deduplicates_by_stable_profile_id() {
        // Two distinct profiles (distinct ids) produce two saved-profile nodes,
        // even if they share a display name.
        let store = temp_store();
        store.add(ConnectionStore::create_sql_login(
            "dup", "s1", 1433, None, "sa", "pw", true, false,
        ));
        store.add(ConnectionStore::create_sql_login(
            "dup", "s2", 1433, None, "sa", "pw", true, false,
        ));
        let tree = ConnectionManagement::build_tree_from(&store);
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|n| n.kind == ConnectionManagementNodeKind::SavedProfile)
                .count(),
            2
        );
    }

    #[test]
    fn via_relay_profile_gets_a_relay_badge() {
        use sqlly_core::connection::ConnectionRoute;
        let store = temp_store();
        let mut profile = ConnectionStore::create_sql_login(
            "Relayed",
            "db.internal",
            1433,
            None,
            "sa",
            "pw",
            true,
            false,
        );
        profile.route = ConnectionRoute::ViaRelay {
            relay_device_id: "dev-1".to_string(),
            pair_id: "pair-1".to_string(),
            relay_hint: None,
        };
        store.add(profile);

        let tree = ConnectionManagement::build_tree_from(&store);
        let node = tree
            .nodes
            .iter()
            .find(|n| n.kind == ConnectionManagementNodeKind::SavedProfile)
            .expect("saved profile node");
        assert!(
            node.badges.iter().any(|b| b == RELAY_BADGE),
            "expected a \"{RELAY_BADGE}\" badge, got {:?}",
            node.badges
        );
    }

    #[test]
    fn direct_profile_has_no_relay_badge() {
        let store = temp_store();
        store.add(ConnectionStore::create_sql_login(
            "DirectConn",
            "localhost",
            1433,
            None,
            "sa",
            "pw",
            true,
            false,
        ));
        let tree = ConnectionManagement::build_tree_from(&store);
        let node = tree
            .nodes
            .iter()
            .find(|n| n.kind == ConnectionManagementNodeKind::SavedProfile)
            .expect("saved profile node");
        assert!(!node.badges.iter().any(|b| b == RELAY_BADGE));
    }

    fn sample_azure_discovery(
        account: &str,
        sub: &str,
        server: &str,
        db: &str,
    ) -> AzureSqlDiscovery {
        AzureSqlDiscovery {
            account_id: account.to_string(),
            username: format!("user@{}", account),
            tenant_id: format!("tenant-{}", account),
            subscription_id: sub.to_string(),
            subscription_name: Some(format!("Sub {}", sub)),
            resource_group: format!("rg-{}", server),
            server_id: format!("/sub/{}/servers/{}", sub, server),
            server_name: server.to_string(),
            server_fqdn: format!("{}.database.windows.net", server),
            location: "eastus".to_string(),
            database_id: format!("/sub/{}/servers/{}/dbs/{}", sub, server, db),
            database_name: db.to_string(),
            service_tier: Some("GeneralPurpose".to_string()),
            status: Some("Online".to_string()),
            linked_profile_id: None,
        }
    }

    #[test]
    fn build_tree_with_azure_merges_saved_and_discovered() {
        let store = temp_store();
        store.add(ConnectionStore::create_sql_login(
            "MyConn",
            "localhost",
            1433,
            None,
            "sa",
            "pw",
            true,
            false,
        ));
        let _ = ConnectionManagement::build_tree_from(&store); // just exercise it
        let discoveries = vec![sample_azure_discovery("acct", "sub", "srv", "db")];
        let tree = ConnectionManagement::build_tree_with_azure(&discoveries);
        assert!(
            tree.nodes
                .iter()
                .any(|n| n.kind == ConnectionManagementNodeKind::SqlServersRoot)
        );
        assert!(
            tree.nodes
                .iter()
                .any(|n| n.kind == ConnectionManagementNodeKind::AzureRoot)
        );
        assert!(
            tree.nodes
                .iter()
                .any(|n| n.kind == ConnectionManagementNodeKind::AzureDatabase)
        );
    }

    #[test]
    fn build_tree_with_azure_empty_discoveries_has_no_azure_root() {
        let tree = ConnectionManagement::build_tree_with_azure(&[]);
        assert!(
            !tree
                .nodes
                .iter()
                .any(|n| n.kind == ConnectionManagementNodeKind::AzureRoot)
        );
    }

    #[test]
    fn build_tree_with_azure_linked_database_has_linked_source() {
        let mut d = sample_azure_discovery("acct", "sub", "srv", "db");
        d.linked_profile_id = Some(Uuid::nil());
        let tree = ConnectionManagement::build_tree_with_azure(&[d]);
        let db_node = tree
            .nodes
            .iter()
            .find(|n| n.kind == ConnectionManagementNodeKind::AzureDatabase)
            .expect("should have AzureDatabase node");
        assert_eq!(
            db_node.source,
            sqlly_core::ConnectionManagementNodeSource::Linked
        );
    }

    #[test]
    fn compute_split_width_grows_from_default() {
        let scale = Theme::SCALE;
        let w = compute_split_width(250.0, 100.0, scale);
        assert!((w - (250.0 + 100.0 / scale)).abs() < f32::EPSILON);
    }

    #[test]
    fn compute_split_width_clamps_to_min() {
        let scale = Theme::SCALE;
        let w = compute_split_width(250.0, -100_000.0, scale);
        assert_eq!(w, 150.0);
    }

    #[test]
    fn compute_split_width_clamps_to_max() {
        let scale = Theme::SCALE;
        let w = compute_split_width(250.0, 100_000.0, scale);
        assert_eq!(w, 600.0);
    }

    #[test]
    fn compute_split_width_zero_delta_unchanged() {
        let scale = Theme::SCALE;
        let w = compute_split_width(250.0, 0.0, scale);
        assert_eq!(w, 250.0);
    }
}
