//! Manage reusable tunnel profiles (proxy and/or SSH hops).

use std::time::Duration;

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    Styled, Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};
use gpui_component::select::{Select, SelectState};
use sqlly_core::ConnectionProfile;
use sqlly_core::connection::{
    ProxyHop, ProxyKind, SecretProvider, SecretRef, SshTunnelConfig, TransportChain, TunnelProfile,
    TunnelProfileId,
};
use uuid::Uuid;

use crate::connection::ConnectionStore;
use crate::connection_editor::{SshAuthMode, build_ssh_auth, build_ssh_hop};
use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::ToolDialogEvent;

#[derive(Clone, Debug, Default)]
pub struct TunnelProfileForm {
    pub id: Option<TunnelProfileId>,
    pub name: String,
    pub proxy_enabled: bool,
    pub proxy_kind: ProxyKind,
    pub proxy_host: String,
    pub proxy_port: String,
    pub proxy_user: String,
    pub ssh_enabled: bool,
    pub ssh_host: String,
    pub ssh_port: String,
    pub ssh_user: String,
    pub ssh_auth: SshAuthMode,
    pub ssh_key_path: String,
    pub ssh_jump_enabled: bool,
    pub ssh_jump_host: String,
    pub ssh_jump_port: String,
    pub ssh_jump_user: String,
}

impl TunnelProfileForm {
    pub fn validate(&self, existing_names: &[String]) -> Vec<String> {
        let mut errors = Vec::new();
        if self.name.trim().is_empty() {
            errors.push(crate::localization::text(
                "connections.tunnels.name_required",
            ));
        } else if self.id.is_none()
            && existing_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(self.name.trim()))
        {
            errors.push(crate::localization::text(
                "connections.tunnels.duplicate_name",
            ));
        }
        if !self.proxy_enabled && !self.ssh_enabled {
            errors.push(crate::localization::text(
                "connections.tunnels.hop_required",
            ));
        }
        let chain = match self.to_chain(None, None) {
            Ok(c) => c,
            Err(e) => {
                errors.push(e);
                return errors;
            }
        };
        errors.extend(chain.validate());
        errors
    }

    pub fn to_profile(
        &self,
        proxy_secret: Option<SecretRef>,
        ssh_secret: Option<SecretRef>,
    ) -> Result<TunnelProfile, String> {
        let chain = self.to_chain(proxy_secret, ssh_secret)?;
        Ok(TunnelProfile {
            id: self.id.unwrap_or_else(Uuid::new_v4),
            name: self.name.trim().to_string(),
            chain,
        })
    }

    pub fn from_profile(profile: &TunnelProfile) -> Self {
        let mut form = Self {
            id: Some(profile.id),
            name: profile.name.clone(),
            ..Default::default()
        };
        if let Some(proxy) = &profile.chain.proxy {
            form.proxy_enabled = true;
            form.proxy_kind = proxy.kind;
            form.proxy_host = proxy.host.clone();
            form.proxy_port = proxy.port.to_string();
            form.proxy_user = proxy.username.clone().unwrap_or_default();
        }
        if let Some(ssh) = &profile.chain.ssh {
            form.ssh_enabled = true;
            form.ssh_host = ssh.target.host.clone();
            form.ssh_port = ssh.target.port.to_string();
            form.ssh_user = ssh.target.username.clone();
            form.ssh_auth = SshAuthMode::from_ssh_auth(&ssh.target.auth);
            if let sqlly_core::connection::SshAuth::KeyFile { path, .. } = &ssh.target.auth {
                form.ssh_key_path = path.clone();
            }
            if let Some(jump) = &ssh.jump {
                form.ssh_jump_enabled = true;
                form.ssh_jump_host = jump.host.clone();
                form.ssh_jump_port = jump.port.to_string();
                form.ssh_jump_user = jump.username.clone();
            }
        }
        form
    }

    fn to_chain(
        &self,
        proxy_secret: Option<SecretRef>,
        ssh_secret: Option<SecretRef>,
    ) -> Result<TransportChain, String> {
        let proxy = if self.proxy_enabled {
            let host = self.proxy_host.trim();
            if host.is_empty() {
                return Err(crate::localization::text(
                    "connections.tunnels.proxy_host_required",
                ));
            }
            let port =
                self.proxy_port.trim().parse::<u16>().map_err(|_| {
                    crate::localization::text("connections.tunnels.proxy_port_invalid")
                })?;
            if port == 0 {
                return Err(crate::localization::text(
                    "connections.tunnels.proxy_port_invalid",
                ));
            }
            let username = {
                let u = self.proxy_user.trim();
                if u.is_empty() {
                    None
                } else {
                    Some(u.to_string())
                }
            };
            Some(ProxyHop {
                kind: self.proxy_kind,
                host: host.to_string(),
                port,
                username,
                password: proxy_secret,
            })
        } else {
            None
        };
        let ssh = if self.ssh_enabled {
            let auth = build_ssh_auth(
                self.ssh_auth,
                &self.ssh_key_path,
                "",
                ssh_secret
                    .as_ref()
                    .and_then(|r| Uuid::parse_str(&r.key).ok()),
                true,
            )?;
            let target =
                build_ssh_hop(&self.ssh_host, &self.ssh_port, &self.ssh_user, auth.clone())?;
            let jump = if self.ssh_jump_enabled {
                Some(build_ssh_hop(
                    &self.ssh_jump_host,
                    &self.ssh_jump_port,
                    &self.ssh_jump_user,
                    auth,
                )?)
            } else {
                None
            };
            Some(SshTunnelConfig {
                target,
                jump,
                keepalive: Default::default(),
            })
        } else {
            None
        };
        Ok(TransportChain {
            proxy,
            ssh,
            kube: None,
            ssm: None,
        })
    }

    #[cfg(test)]
    pub fn apply_ssh_config_host(&mut self, host: &sqlly_engines::ssh::SshConfigHost) {
        let hop = host.to_hop();
        self.ssh_enabled = true;
        self.ssh_host = hop.host;
        self.ssh_port = hop.port.to_string();
        self.ssh_user = hop.username;
        self.ssh_auth = SshAuthMode::from_ssh_auth(&hop.auth);
        if let sqlly_core::connection::SshAuth::KeyFile { path, .. } = hop.auth {
            self.ssh_key_path = path;
        }
    }
}

/// An armed tunnel-profile deletion awaiting confirmation. Snapshotted from
/// the store when Delete is clicked, so the confirmation names exactly what is
/// about to go and which saved connections lose their tunnel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingTunnelDeletion {
    pub(crate) id: TunnelProfileId,
    pub(crate) name: String,
    /// Saved connections that route through this profile, sorted by name.
    pub(crate) used_by: Vec<String>,
}

impl PendingTunnelDeletion {
    /// Reads, never writes: arming a deletion must leave the store untouched.
    /// A profile that no longer exists arms nothing.
    pub(crate) fn arm(store: &ConnectionStore, id: TunnelProfileId) -> Option<Self> {
        let profile = store.tunnel_profile(id)?;
        let mut used_by: Vec<String> = store
            .list()
            .into_iter()
            .filter(|c| c.tunnel_profile == Some(id))
            .map(|c| c.name)
            .collect();
        used_by.sort_by_key(|name| name.to_lowercase());
        Some(Self {
            id,
            name: profile.name,
            used_by,
        })
    }

    /// What deleting does, in the words the confirmation shows. Connections
    /// that used the profile keep working, but with no tunnel at all, so they
    /// are named rather than counted.
    pub(crate) fn impact(&self) -> String {
        let (key, arguments) = match self.used_by.as_slice() {
            [] => ("connections.tunnels.delete_impact_none", Vec::new()),
            [only] => (
                "connections.tunnels.delete_impact_one",
                vec![("connection_name", only.clone())],
            ),
            many => (
                "connections.tunnels.delete_impact_many",
                vec![
                    ("count", many.len().to_string()),
                    (
                        "connection_names",
                        many.iter()
                            .map(|name| format!("\"{name}\""))
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                ],
            ),
        };
        let arguments: Vec<(&str, &str)> = arguments
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        crate::localization::format(key, &arguments)
    }
}

/// Keyring entries a tunnel profile owns: the proxy password and the SSH
/// password or key passphrase. Deleting the profile must not orphan them.
fn delete_tunnel_profile_secrets(profile: &TunnelProfile) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Some(secret) = profile
        .chain
        .proxy
        .as_ref()
        .and_then(|p| p.password.as_ref())
        && secret.provider == SecretProvider::MacOsKeychain
        && let Ok(id) = Uuid::parse_str(&secret.key)
        && let Err(e) = crate::sql_credentials::delete_password(id)
    {
        errors.push(e);
    }
    if let Some(ssh) = &profile.chain.ssh
        && let Err(e) = crate::sql_credentials::delete_ssh_secrets(ssh)
    {
        errors.push(e);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub struct TunnelProfilesDialog {
    probe_profile: Option<ConnectionProfile>,
    selected_id: Option<TunnelProfileId>,
    form: TunnelProfileForm,
    proxy_secret_id: Option<Uuid>,
    ssh_secret_id: Option<Uuid>,
    name_input: Entity<InputState>,
    proxy_host_input: Entity<InputState>,
    proxy_port_input: Entity<InputState>,
    proxy_user_input: Entity<InputState>,
    proxy_secret_input: Entity<InputState>,
    proxy_kind_select: Entity<SelectState<Vec<String>>>,
    ssh_host_input: Entity<InputState>,
    ssh_port_input: Entity<InputState>,
    ssh_user_input: Entity<InputState>,
    ssh_key_input: Entity<InputState>,
    ssh_secret_input: Entity<InputState>,
    ssh_auth_select: Entity<SelectState<Vec<String>>>,
    ssh_config_select: Entity<SelectState<Vec<String>>>,
    ssh_jump_host_input: Entity<InputState>,
    ssh_jump_port_input: Entity<InputState>,
    ssh_jump_user_input: Entity<InputState>,
    status_line: Option<String>,
    status_error: bool,
    pending_deletion: Option<PendingTunnelDeletion>,
    focus: FocusHandle,
    frame: DialogFrame,
    deletion_frame: DialogFrame,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ToolDialogEvent> for TunnelProfilesDialog {}

impl TunnelProfilesDialog {
    pub fn new(
        probe_profile: Option<ConnectionProfile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let name_input = cx.new(|cx| InputState::new(window, cx));
        let proxy_host_input = cx.new(|cx| InputState::new(window, cx));
        let proxy_port_input = cx.new(|cx| InputState::new(window, cx));
        let proxy_user_input = cx.new(|cx| InputState::new(window, cx));
        let proxy_secret_input = cx.new(|cx| InputState::new(window, cx).masked(true));
        let proxy_kind_select = cx.new(|cx| {
            let mut s = SelectState::new(
                vec!["SOCKS5".to_string(), "HTTP CONNECT".to_string()],
                None,
                window,
                cx,
            );
            s.set_selected_value(&"SOCKS5".to_string(), window, cx);
            s
        });
        let ssh_host_input = cx.new(|cx| InputState::new(window, cx));
        let ssh_port_input = cx.new(|cx| InputState::new(window, cx));
        let ssh_user_input = cx.new(|cx| InputState::new(window, cx));
        let ssh_key_input = cx.new(|cx| InputState::new(window, cx));
        let ssh_secret_input = cx.new(|cx| InputState::new(window, cx).masked(true));
        let ssh_auth_select = cx.new(|cx| {
            let mut s = SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    SshAuthMode::SELECT_ORDER
                        .into_iter()
                        .map(|mode| {
                            crate::localization::text(match mode {
                                SshAuthMode::Agent => "connections.tunnels.ssh_agent_option",
                                SshAuthMode::KeyFile => "connections.tunnels.private_key_option",
                                SshAuthMode::Password => "connections.tunnels.password_option",
                            })
                        })
                        .collect()
                }),
                None,
                window,
                cx,
            );
            s.set_selected_index(Some(gpui_component::IndexPath::new(0)), window, cx);
            s
        });
        let ssh_hosts: Vec<String> = sqlly_engines::ssh::load_user_ssh_config()
            .into_iter()
            .map(|h| h.alias)
            .collect();
        let ssh_config_select = cx.new(|cx| {
            let mut items = vec![crate::localization::text(
                "connections.tunnels.import_ssh_config_option",
            )];
            items.extend(ssh_hosts);
            SelectState::new(items, None, window, cx)
        });
        let ssh_jump_host_input = cx.new(|cx| InputState::new(window, cx));
        let ssh_jump_port_input = cx.new(|cx| InputState::new(window, cx));
        let ssh_jump_user_input = cx.new(|cx| InputState::new(window, cx));

        Self {
            probe_profile,
            selected_id: None,
            form: TunnelProfileForm::default(),
            proxy_secret_id: None,
            ssh_secret_id: None,
            name_input,
            proxy_host_input,
            proxy_port_input,
            proxy_user_input,
            proxy_secret_input,
            proxy_kind_select,
            ssh_host_input,
            ssh_port_input,
            ssh_user_input,
            ssh_key_input,
            ssh_secret_input,
            ssh_auth_select,
            ssh_config_select,
            ssh_jump_host_input,
            ssh_jump_port_input,
            ssh_jump_user_input,
            status_line: None,
            status_error: false,
            pending_deletion: None,
            focus,
            frame: DialogFrame::new(Theme::scaled(760.0), Theme::scaled(560.0)),
            deletion_frame: DialogFrame::new_auto(Theme::scaled(400.0)),
            _subs: Vec::new(),
        }
    }

    fn existing_names(&self) -> Vec<String> {
        ConnectionStore::global()
            .tunnel_profiles()
            .into_iter()
            .filter(|p| Some(p.id) != self.selected_id)
            .map(|p| p.name)
            .collect()
    }

    fn sync_form_from_inputs(&mut self, cx: &Context<Self>) {
        self.form.name = self.name_input.read(cx).value().to_string();
        self.form.proxy_host = self.proxy_host_input.read(cx).value().to_string();
        self.form.proxy_port = self.proxy_port_input.read(cx).value().to_string();
        self.form.proxy_user = self.proxy_user_input.read(cx).value().to_string();
        self.form.ssh_host = self.ssh_host_input.read(cx).value().to_string();
        self.form.ssh_port = self.ssh_port_input.read(cx).value().to_string();
        self.form.ssh_user = self.ssh_user_input.read(cx).value().to_string();
        self.form.ssh_key_path = self.ssh_key_input.read(cx).value().to_string();
        self.form.ssh_jump_host = self.ssh_jump_host_input.read(cx).value().to_string();
        self.form.ssh_jump_port = self.ssh_jump_port_input.read(cx).value().to_string();
        self.form.ssh_jump_user = self.ssh_jump_user_input.read(cx).value().to_string();
        let kind = self
            .proxy_kind_select
            .read(cx)
            .selected_value()
            .cloned()
            .unwrap_or_else(|| "SOCKS5".to_string());
        self.form.proxy_kind = if kind == "HTTP CONNECT" {
            ProxyKind::HttpConnect
        } else {
            ProxyKind::Socks5
        };
        // The labels follow `SshAuthMode::SELECT_ORDER`; dispatch on the row,
        // never the translated label.
        self.form.ssh_auth = self
            .ssh_auth_select
            .read(cx)
            .selected_index(cx)
            .and_then(|ix| SshAuthMode::from_select_row(ix.row))
            .unwrap_or(SshAuthMode::Agent);
    }

    fn load_profile(
        &mut self,
        profile: &TunnelProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_id = Some(profile.id);
        self.form = TunnelProfileForm::from_profile(profile);
        self.name_input
            .update(cx, |s, cx| s.set_value(&self.form.name, window, cx));
        self.proxy_host_input
            .update(cx, |s, cx| s.set_value(&self.form.proxy_host, window, cx));
        self.proxy_port_input.update(cx, |s, cx| {
            s.set_value(
                if self.form.proxy_enabled {
                    self.form.proxy_port.clone()
                } else {
                    "1080".to_string()
                },
                window,
                cx,
            )
        });
        self.proxy_user_input
            .update(cx, |s, cx| s.set_value(&self.form.proxy_user, window, cx));
        self.ssh_host_input
            .update(cx, |s, cx| s.set_value(&self.form.ssh_host, window, cx));
        self.ssh_port_input.update(cx, |s, cx| {
            s.set_value(
                if self.form.ssh_port.is_empty() {
                    "22".to_string()
                } else {
                    self.form.ssh_port.clone()
                },
                window,
                cx,
            )
        });
        self.ssh_user_input
            .update(cx, |s, cx| s.set_value(&self.form.ssh_user, window, cx));
        self.ssh_key_input
            .update(cx, |s, cx| s.set_value(&self.form.ssh_key_path, window, cx));
        self.proxy_secret_id = profile
            .chain
            .proxy
            .as_ref()
            .and_then(|p| p.password.as_ref())
            .and_then(|r| Uuid::parse_str(&r.key).ok());
        self.ssh_secret_id = profile
            .chain
            .ssh
            .as_ref()
            .and_then(|t| match &t.target.auth {
                sqlly_core::connection::SshAuth::Password { secret_ref } => {
                    Uuid::parse_str(&secret_ref.key).ok()
                }
                sqlly_core::connection::SshAuth::KeyFile { passphrase, .. } => passphrase
                    .as_ref()
                    .and_then(|r| Uuid::parse_str(&r.key).ok()),
                _ => None,
            });
        self.sync_choice_selects(window, cx);
        cx.notify();
    }

    /// Show the form's proxy kind and SSH auth mode in their selects, so a
    /// later `sync_form_from_inputs` reads back what was loaded.
    fn sync_choice_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let proxy_row = match self.form.proxy_kind {
            ProxyKind::Socks5 => 0,
            ProxyKind::HttpConnect => 1,
        };
        self.proxy_kind_select.update(cx, |s, cx| {
            s.set_selected_index(Some(gpui_component::IndexPath::new(proxy_row)), window, cx)
        });
        let auth_row = SshAuthMode::SELECT_ORDER
            .iter()
            .position(|mode| *mode == self.form.ssh_auth)
            .unwrap_or(0);
        self.ssh_auth_select.update(cx, |s, cx| {
            s.set_selected_index(Some(gpui_component::IndexPath::new(auth_row)), window, cx)
        });
    }

    fn handle_save(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.sync_form_from_inputs(cx);
        let errors = self.form.validate(&self.existing_names());
        if !errors.is_empty() {
            self.status_line = Some(errors.join("; "));
            self.status_error = true;
            cx.notify();
            return;
        }
        let proxy_secret = self.build_proxy_secret(cx);
        let ssh_secret = self.build_ssh_secret(cx);
        let profile = match self
            .form
            .to_profile(proxy_secret.clone(), ssh_secret.clone())
        {
            Ok(p) => p,
            Err(e) => {
                self.status_line = Some(e);
                self.status_error = true;
                cx.notify();
                return;
            }
        };
        if let Some(secret) = proxy_secret
            && let Ok(id) = Uuid::parse_str(&secret.key)
        {
            let typed = self.proxy_secret_input.read(cx).value();
            if !typed.trim().is_empty() && typed != crate::connection_editor::password_mask() {
                let _ = crate::sql_credentials::store_password(id, typed.trim());
            }
        }
        if let Some(secret) = ssh_secret
            && let Ok(id) = Uuid::parse_str(&secret.key)
        {
            let typed = self.ssh_secret_input.read(cx).value();
            if !typed.trim().is_empty() && typed != crate::connection_editor::password_mask() {
                let _ = crate::sql_credentials::store_password(id, typed.trim());
            }
        }
        ConnectionStore::global().upsert_tunnel_profile(profile);
        self.status_line = Some(crate::localization::text("connections.tunnels.saved"));
        self.status_error = false;
        cx.notify();
    }

    fn build_proxy_secret(&self, cx: &Context<Self>) -> Option<SecretRef> {
        if !self.form.proxy_enabled {
            return None;
        }
        let typed = self.proxy_secret_input.read(cx).value();
        if typed.trim().is_empty() || typed == crate::connection_editor::password_mask() {
            return self.proxy_secret_id.map(|id| SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: id.to_string(),
            });
        }
        let id = self.proxy_secret_id.unwrap_or_else(Uuid::new_v4);
        Some(SecretRef {
            provider: SecretProvider::MacOsKeychain,
            key: id.to_string(),
        })
    }

    fn build_ssh_secret(&self, cx: &Context<Self>) -> Option<SecretRef> {
        if !self.form.ssh_enabled {
            return None;
        }
        if !matches!(
            self.form.ssh_auth,
            SshAuthMode::Password | SshAuthMode::KeyFile
        ) {
            return None;
        }
        let typed = self.ssh_secret_input.read(cx).value();
        if typed.trim().is_empty() || typed == crate::connection_editor::password_mask() {
            return self.ssh_secret_id.map(|id| SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: id.to_string(),
            });
        }
        let id = self.ssh_secret_id.unwrap_or_else(Uuid::new_v4);
        Some(SecretRef {
            provider: SecretProvider::MacOsKeychain,
            key: id.to_string(),
        })
    }

    /// Delete clicked: arm the confirmation. Nothing is removed until
    /// [`Self::confirm_delete`].
    fn request_delete(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id else {
            return;
        };
        match PendingTunnelDeletion::arm(ConnectionStore::global(), id) {
            Some(pending) => self.pending_deletion = Some(pending),
            None => {
                self.selected_id = None;
                self.status_line = Some(crate::localization::text(
                    "connections.tunnels.profile_missing",
                ));
                self.status_error = true;
            }
        }
        cx.notify();
    }

    /// The user confirmed: remove the profile (clearing it from the
    /// connections that used it), empty the form, and purge its saved
    /// passwords from the keyring off the UI thread.
    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_deletion.take() else {
            return;
        };
        let store = ConnectionStore::global();
        let removed = store.tunnel_profile(pending.id);
        let cleared = store.remove_tunnel_profile(pending.id);
        if self.selected_id == Some(pending.id) {
            self.clear_form(window, cx);
        }
        let key = match cleared {
            0 => "connections.tunnels.deleted_without_connections",
            1 => "connections.tunnels.deleted_one_connection",
            _ => "connections.tunnels.deleted_many_connections",
        };
        let count = cleared.to_string();
        let mut arguments = vec![("profile_name", pending.name.as_str())];
        if cleared > 0 {
            arguments.push(("count", count.as_str()));
        }
        self.status_line = Some(crate::localization::format(key, &arguments));
        self.status_error = false;
        if let Some(profile) = removed {
            let purge = cx
                .background_executor()
                .spawn(async move { delete_tunnel_profile_secrets(&profile) });
            cx.spawn(async move |this, cx| {
                if let Err(e) = purge.await {
                    let _ = this.update(cx, |dialog, cx| {
                        dialog.status_line = Some(crate::localization::format(
                            "connections.tunnels.deleted_password_cleanup_failed",
                            &[("profile_name", &pending.name), ("error", &e)],
                        ));
                        dialog.status_error = true;
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        cx.notify();
    }

    fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        if self.pending_deletion.take().is_some() {
            cx.notify();
        }
    }

    /// Back to an empty, unselected form. The secret ids go too, so a later
    /// Save cannot write a new profile's password over a deleted one's entry.
    fn clear_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_id = None;
        self.form = TunnelProfileForm::default();
        self.proxy_secret_id = None;
        self.ssh_secret_id = None;
        for input in [
            &self.name_input,
            &self.proxy_host_input,
            &self.proxy_port_input,
            &self.proxy_user_input,
            &self.proxy_secret_input,
            &self.ssh_host_input,
            &self.ssh_port_input,
            &self.ssh_user_input,
            &self.ssh_key_input,
            &self.ssh_secret_input,
            &self.ssh_jump_host_input,
            &self.ssh_jump_port_input,
            &self.ssh_jump_user_input,
        ] {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.sync_choice_selects(window, cx);
    }

    fn handle_test(&mut self, cx: &mut Context<Self>) {
        self.sync_form_from_inputs(cx);
        let chain = match self
            .form
            .to_chain(self.build_proxy_secret(cx), self.build_ssh_secret(cx))
        {
            Ok(c) => c,
            Err(e) => {
                self.status_line = Some(e);
                self.status_error = true;
                cx.notify();
                return;
            }
        };
        let (target_host, target_port) = if let Some(probe) = &self.probe_profile {
            (probe.server.trim().to_string(), probe.port)
        } else if let Some(ssh) = &chain.ssh {
            (ssh.target.host.clone(), ssh.target.port)
        } else {
            self.status_line = Some(crate::localization::text(
                "connections.tunnels.no_ssh_target",
            ));
            self.status_error = true;
            cx.notify();
            return;
        };
        let handle = crate::tokio_handle();
        let chain = chain.clone();
        let async_cx = cx.to_async();
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let result = tokio::task::spawn_blocking(move || {
                handle.block_on(sqlly_engines::ssh::open_chain(
                    &chain,
                    &target_host,
                    target_port,
                    Duration::from_secs(10),
                ))
            })
            .await;
            let mut async_cx = async_cx.clone();
            let _ = weak.update(&mut async_cx, |dialog, cx| {
                match result {
                    Ok(Ok(handle)) => {
                        dialog.status_line = Some(crate::localization::format(
                            "connections.tunnels.test_opened",
                            &[("local_port", &handle.local_port.to_string())],
                        ));
                        dialog.status_error = false;
                        drop(handle);
                    }
                    Ok(Err(e)) => {
                        dialog.status_line = Some(e.to_string());
                        dialog.status_error = true;
                    }
                    Err(e) => {
                        dialog.status_line = Some(e.to_string());
                        dialog.status_error = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Focusable for TunnelProfilesDialog {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TunnelProfilesDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let profiles = ConnectionStore::global().tunnel_profiles();
        let entity = cx.entity().clone();

        // `AppView` stacks this entity after the full-size workbench, so it
        // renders through the dialog chrome's overlay; a plain block would
        // be laid out below the window.
        let content = div()
            .id("tunnel-profiles-content")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .p(px(s(12.0)))
            .gap(px(s(8.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(12.0)))
                    .flex_1()
                    .child(
                        div()
                            .w(px(s(180.0)))
                            .flex()
                            .flex_col()
                            .gap(px(s(4.0)))
                            .children(profiles.iter().enumerate().map(|(idx, p)| {
                                let profile = p.clone();
                                let e = entity.clone();
                                Button::new(format!("tp-list-{idx}"))
                                    .ghost()
                                    .label(p.name.clone())
                                    .on_click(move |_, window, cx| {
                                        e.update(cx, |dialog, cx| {
                                            dialog.load_profile(&profile, window, cx);
                                        });
                                    })
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(s(6.0)))
                            .child(
                                div()
                                    .child(crate::localization::text(
                                        "connections.tunnels.name_label",
                                    ))
                                    .child(Input::new(&self.name_input)),
                            )
                            .child(
                                Checkbox::new("tp-proxy")
                                    .checked(self.form.proxy_enabled)
                                    .label(crate::localization::text(
                                        "connections.tunnels.use_proxy_label",
                                    ))
                                    .on_click(cx.listener(|this, checked, _, cx| {
                                        this.form.proxy_enabled = *checked;
                                        cx.notify();
                                    })),
                            )
                            .when(self.form.proxy_enabled, |d| {
                                d.child(Select::new(&self.proxy_kind_select))
                                    .child(Input::new(&self.proxy_host_input))
                                    .child(Input::new(&self.proxy_port_input))
                                    .child(Input::new(&self.proxy_user_input))
                                    .child(Input::new(&self.proxy_secret_input).mask_toggle())
                            })
                            .child(
                                Checkbox::new("tp-ssh")
                                    .checked(self.form.ssh_enabled)
                                    .label(crate::localization::text(
                                        "connections.tunnels.use_ssh_label",
                                    ))
                                    .on_click(cx.listener(|this, checked, _, cx| {
                                        this.form.ssh_enabled = *checked;
                                        cx.notify();
                                    })),
                            )
                            .when(self.form.ssh_enabled, |d| {
                                d.child(Select::new(&self.ssh_config_select))
                                    .child(Input::new(&self.ssh_host_input))
                                    .child(Input::new(&self.ssh_port_input))
                                    .child(Input::new(&self.ssh_user_input))
                                    .child(Select::new(&self.ssh_auth_select))
                                    .child(Input::new(&self.ssh_key_input))
                                    .child(Input::new(&self.ssh_secret_input).mask_toggle())
                            }),
                    ),
            )
            .when_some(self.status_line.clone(), |d, line| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(if self.status_error {
                            Theme::error()
                        } else {
                            Theme::text_dim()
                        })
                        .child(line),
                )
            });

        let focus = self.focus.clone();
        let main = Dialog::new("tunnel-profiles-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text(
                "connections.tunnels.dialog_title",
            ))
            .key_context("TunnelProfiles")
            .track_focus(&focus)
            .bare_body()
            // Escape while the delete confirmation is up backs out of the
            // confirmation, not the whole dialog.
            .on_close(|this, _window, cx| {
                if this.pending_deletion.is_some() {
                    this.cancel_delete(cx);
                } else {
                    cx.emit(ToolDialogEvent::Close);
                }
            })
            .child(content)
            .leading_button(
                Button::new("tp-delete")
                    .label(crate::localization::text(
                        "connections.tunnels.delete_button",
                    ))
                    .disabled(self.selected_id.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.request_delete(cx))),
            )
            .button(
                Button::new("tp-test")
                    .label(crate::localization::text("connections.tunnels.test_button"))
                    .on_click(cx.listener(|this, _, _, cx| this.handle_test(cx))),
            )
            .button(
                Button::new("tp-save")
                    .primary()
                    .label(crate::localization::text("connections.tunnels.save_button"))
                    .on_click(cx.listener(|this, _, window, cx| this.handle_save(window, cx))),
            )
            .build(self, window, cx);

        let Some(pending) = self.pending_deletion.clone() else {
            return div().absolute().top_0().left_0().size_full().child(main);
        };
        // Same chrome as the workbench's Delete Connection confirmation,
        // stacked above this dialog.
        let confirm = Dialog::new("tunnel-profile-deletion-confirm", |this: &mut Self| {
            &mut this.deletion_frame
        })
        .title(crate::localization::text(
            "connections.tunnels.delete_confirmation_title",
        ))
        .confirm(
            crate::localization::text("connections.tunnels.delete_confirm_button"),
            crate::localization::text("connections.tunnels.delete_cancel_button"),
        )
        .danger()
        .dismiss_on_scrim_click(true)
        .on_confirm(|this, window, cx| this.confirm_delete(window, cx))
        .on_reject(|this, _window, cx| this.cancel_delete(cx))
        .on_close(|this, _window, cx| this.cancel_delete(cx))
        .child(
            div()
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(Theme::text())
                .child(crate::localization::format(
                    "connections.tunnels.delete_confirmation_question",
                    &[("profile_name", &pending.name)],
                )),
        )
        .child(
            div()
                .debug_selector(|| "tunnel-deletion-impact".to_string())
                .text_size(px(Theme::font_size_ratio(0.80)))
                .text_color(if pending.used_by.is_empty() {
                    Theme::text_dim()
                } else {
                    Theme::warning()
                })
                .child(pending.impact()),
        )
        .build(self, window, cx);
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(main)
            .child(confirm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn form_requires_name_and_at_least_one_hop() {
        let form = TunnelProfileForm::default();
        let errors = form.validate(&[]);
        assert!(errors.iter().any(|e| e.contains("Name")));
        assert!(errors.iter().any(|e| e.contains("at least one")));
    }

    #[test]
    fn form_rejects_duplicate_name() {
        let mut form = TunnelProfileForm {
            name: "corp".to_string(),
            ssh_enabled: true,
            ssh_host: "bastion".to_string(),
            ssh_port: "22".to_string(),
            ssh_user: "u".to_string(),
            ..Default::default()
        };
        let errors = form.validate(&["corp".to_string()]);
        assert!(errors.iter().any(|e| e.contains("already exists")));
        form.id = Some(Uuid::new_v4());
        let errors = form.validate(&["corp".to_string()]);
        assert!(errors.is_empty());
    }

    #[test]
    fn form_round_trips_profile() {
        let profile = TunnelProfile {
            id: Uuid::new_v4(),
            name: "hop".to_string(),
            chain: TransportChain {
                proxy: Some(ProxyHop {
                    kind: ProxyKind::Socks5,
                    host: "proxy".to_string(),
                    port: 1080,
                    username: None,
                    password: None,
                }),
                ssh: None,
                kube: None,
                ssm: None,
            },
        };
        let form = TunnelProfileForm::from_profile(&profile);
        let restored = form.to_profile(None, None).unwrap();
        assert_eq!(restored.name, profile.name);
        assert_eq!(restored.chain.proxy, profile.chain.proxy);
    }

    fn temp_store_with_tunnel(users: &[&str]) -> (ConnectionStore, TunnelProfileId, PathBuf) {
        let path = std::env::temp_dir().join(format!("sqlly-tunnel-delete-{}", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&path);
        let tunnel_id = Uuid::new_v4();
        store.upsert_tunnel_profile(TunnelProfile {
            id: tunnel_id,
            name: "bastion".to_string(),
            chain: TransportChain {
                proxy: Some(ProxyHop {
                    kind: ProxyKind::Socks5,
                    host: "proxy".to_string(),
                    port: 1080,
                    username: None,
                    password: None,
                }),
                ssh: None,
                kube: None,
                ssm: None,
            },
        });
        for name in users {
            let mut connection = ConnectionProfile::sql_login(
                *name,
                "db.internal",
                "sa",
                SecretRef {
                    provider: SecretProvider::InMemory,
                    key: "pw".to_string(),
                },
            );
            connection.tunnel_profile = Some(tunnel_id);
            store.add(connection);
        }
        // A connection on no tunnel must not be named in the confirmation.
        store.add(ConnectionProfile::sql_login(
            "Direct",
            "db.local",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".to_string(),
            },
        ));
        (store, tunnel_id, path)
    }

    /// Delete used to remove the profile on the first click. Arming the
    /// deletion must leave the store untouched and name every connection that
    /// would silently lose its tunnel.
    #[test]
    fn arming_a_tunnel_deletion_removes_nothing_and_names_its_connections() {
        let (store, id, path) = temp_store_with_tunnel(&["Warehouse", "analytics"]);
        let pending = PendingTunnelDeletion::arm(&store, id).expect("profile exists");
        assert_eq!(pending.name, "bastion");
        assert_eq!(pending.used_by, vec!["analytics", "Warehouse"]);
        assert!(
            store.tunnel_profile(id).is_some(),
            "arming must not delete the profile"
        );
        assert!(
            store
                .list()
                .iter()
                .filter(|c| c.name != "Direct")
                .all(|c| c.tunnel_profile == Some(id)),
            "arming must not detach any connection"
        );
        let impact = pending.impact();
        assert!(impact.starts_with("2 connections use this tunnel profile"));
        assert!(impact.contains("\"analytics\", \"Warehouse\""));
        assert!(impact.contains("no tunnel"));
        assert!(!impact.contains("Direct"));

        // Only the confirmed path removes it and detaches its connections.
        assert_eq!(store.remove_tunnel_profile(pending.id), 2);
        assert!(PendingTunnelDeletion::arm(&store, id).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tunnel_deletion_impact_covers_one_and_no_connections() {
        let (store, id, path) = temp_store_with_tunnel(&["Warehouse"]);
        let one = PendingTunnelDeletion::arm(&store, id).unwrap();
        assert!(
            one.impact()
                .starts_with("The connection \"Warehouse\" uses this tunnel profile")
        );
        let none = PendingTunnelDeletion {
            used_by: Vec::new(),
            ..one
        };
        assert!(
            none.impact()
                .starts_with("No saved connection uses this tunnel profile")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn form_ssh_config_import_fills_host_port_user_key() {
        let host = sqlly_engines::ssh::SshConfigHost {
            alias: "myhost".to_string(),
            host_name: Some("real.host".to_string()),
            port: Some(2222),
            user: Some("deploy".to_string()),
            identity_file: Some("/home/me/.ssh/id".to_string()),
        };
        let mut form = TunnelProfileForm::default();
        form.apply_ssh_config_host(&host);
        assert!(form.ssh_enabled);
        assert_eq!(form.ssh_host, "real.host");
        assert_eq!(form.ssh_port, "2222");
        assert_eq!(form.ssh_user, "deploy");
        assert_eq!(form.ssh_key_path, "/home/me/.ssh/id");
    }
}
