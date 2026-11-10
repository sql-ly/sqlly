//! The connection editor's Redis Sentinel fields: a "Connect through
//! Sentinel" switch, the master name, the sentinel host list, the Sentinel
//! password, and "Prefer replica for reads". A child module of
//! `connection_editor` so it can reach the editor's state.
//!
//! The Sentinel password lives in the keyring like the SQL password: a
//! saved profile carries only a keyring reference, minted on first save and
//! kept across edits; the masked placeholder in the field means "keep the
//! saved one". Test Connection uses the typed password in memory.

#[cfg(not(target_arch = "wasm32"))]
use gpui::{AnyElement, div, px};
use gpui::{Context, Entity, Window, prelude::*};
#[cfg(not(target_arch = "wasm32"))]
use gpui_component::input::Input;
use gpui_component::input::InputState;
use sqlly_core::{
    ConnectionProfile, DatabaseType, ParsedConnectionUrl, RedisSentinelConfig, SecretProvider,
    SecretRef,
};
use sqlly_lang::sqlmodel::ConnectionKind;
use uuid::Uuid;

use super::{ConnectionEditor, password_mask};
#[cfg(not(target_arch = "wasm32"))]
use crate::theme::Theme;

/// The Sentinel section's inputs and switches.
pub(super) struct SentinelFields {
    pub(super) enabled: bool,
    pub(super) prefer_replica_reads: bool,
    master_input: Entity<InputState>,
    sentinels_input: Entity<InputState>,
    password_input: Entity<InputState>,
    /// The keyring reference of a saved profile's Sentinel password.
    saved_password_ref: Option<SecretRef>,
}

/// Sentinel applies to the Redis-protocol engines only.
pub(super) fn applies_to(engine: DatabaseType) -> bool {
    engine.connection_kind() == ConnectionKind::Resp
}

/// What the typed Sentinel password means for the profile being built.
fn password_ref_for(
    typed: &str,
    saved: Option<&SecretRef>,
    persistable: bool,
) -> Option<SecretRef> {
    let mask = password_mask();
    if typed.is_empty() {
        return None;
    }
    if typed == mask {
        return saved.cloned();
    }
    if !persistable {
        return Some(SecretRef {
            provider: SecretProvider::InMemory,
            key: typed.to_string(),
        });
    }
    Some(match saved {
        Some(r) if r.provider == SecretProvider::MacOsKeychain => r.clone(),
        _ => SecretRef {
            provider: SecretProvider::MacOsKeychain,
            key: Uuid::new_v4().to_string(),
        },
    })
}

/// The profile's Sentinel settings from the form, `None` when the switch is
/// off. Blank or malformed fields are an error naming what is missing.
pub(super) fn build_config(
    enabled: bool,
    master_name: &str,
    sentinels: &str,
    password_ref: Option<SecretRef>,
    prefer_replica_reads: bool,
) -> Result<Option<RedisSentinelConfig>, String> {
    if !enabled {
        return Ok(None);
    }
    let config = RedisSentinelConfig {
        master_name: master_name.trim().to_string(),
        sentinels: sqlly_core::redis_sentinel::split_sentinel_list(sentinels),
        sentinel_password_ref: password_ref,
        prefer_replica_reads,
    };
    if config.master_name.trim().is_empty() {
        return Err(crate::localization::text(
            "connections.editor.sentinel_master_required",
        ));
    }
    if config.sentinels.is_empty() {
        return Err(crate::localization::text(
            "connections.editor.sentinel_host_required",
        ));
    }
    match config.problem() {
        Some(problem) => Err(problem),
        None => Ok(Some(config)),
    }
}

impl SentinelFields {
    pub(super) fn new(window: &mut Window, cx: &mut Context<ConnectionEditor>) -> Self {
        Self {
            enabled: false,
            prefer_replica_reads: false,
            master_input: cx.new(|cx| InputState::new(window, cx).placeholder("mymaster")),
            sentinels_input: cx.new(|cx| {
                InputState::new(window, cx).placeholder("10.0.0.1:26379, 10.0.0.2:26379")
            }),
            password_input: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::localization::input_placeholder(
                        window,
                        cx,
                        "connections.editor.sentinel_password_placeholder",
                    ))
                    .masked(true)
            }),
            saved_password_ref: None,
        }
    }

    /// Show a saved profile's Sentinel settings.
    pub(super) fn load(
        &mut self,
        config: Option<&RedisSentinelConfig>,
        window: &mut Window,
        cx: &mut Context<ConnectionEditor>,
    ) {
        let Some(config) = config else {
            return;
        };
        self.enabled = true;
        self.prefer_replica_reads = config.prefer_replica_reads;
        self.saved_password_ref = config.sentinel_password_ref.clone();
        let master = config.master_name.clone();
        let sentinels = config.sentinels.join(", ");
        let password = if config.sentinel_password_ref.is_some() {
            password_mask()
        } else {
            String::new()
        };
        self.master_input
            .update(cx, |s, cx| s.set_value(master, window, cx));
        self.sentinels_input
            .update(cx, |s, cx| s.set_value(sentinels, window, cx));
        self.password_input
            .update(cx, |s, cx| s.set_value(password, window, cx));
    }

    /// Fill from a pasted `redis+sentinel://` URL.
    pub(super) fn prefill(
        &mut self,
        parsed: &ParsedConnectionUrl,
        window: &mut Window,
        cx: &mut Context<ConnectionEditor>,
    ) {
        let Some(config) = parsed.redis_sentinel.as_ref() else {
            // A plain redis:// URL describes a direct connection.
            if applies_to(parsed.engine) {
                self.enabled = false;
            }
            return;
        };
        self.enabled = true;
        self.prefer_replica_reads = config.prefer_replica_reads;
        let master = config.master_name.clone();
        let sentinels = config.sentinels.join(", ");
        self.master_input
            .update(cx, |s, cx| s.set_value(master, window, cx));
        self.sentinels_input
            .update(cx, |s, cx| s.set_value(sentinels, window, cx));
        if let Some(password) = parsed.sentinel_password.clone() {
            self.password_input
                .update(cx, |s, cx| s.set_value(password, window, cx));
        }
    }

    /// The Sentinel settings for the profile the form describes, or `None`
    /// when the engine is not Redis-family or the switch is off.
    pub(super) fn config(
        &self,
        engine: DatabaseType,
        persistable: bool,
        cx: &gpui::App,
    ) -> Result<Option<RedisSentinelConfig>, String> {
        if !applies_to(engine) {
            return Ok(None);
        }
        let typed = self.password_input.read(cx).value().to_string();
        build_config(
            self.enabled,
            self.master_input.read(cx).value().as_ref(),
            self.sentinels_input.read(cx).value().as_ref(),
            password_ref_for(&typed, self.saved_password_ref.as_ref(), persistable),
            self.prefer_replica_reads,
        )
    }

    /// On Save: write a newly typed Sentinel password under the profile's
    /// keyring reference (the mask or a blank field writes nothing).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn store_password(
        &self,
        profile: &ConnectionProfile,
        cx: &gpui::App,
    ) -> Result<(), String> {
        let Some(secret_ref) = profile
            .redis_sentinel
            .as_ref()
            .and_then(|s| s.sentinel_password_ref.as_ref())
        else {
            return Ok(());
        };
        if secret_ref.provider != SecretProvider::MacOsKeychain {
            return Ok(());
        }
        let typed = self.password_input.read(cx).value().to_string();
        if typed.is_empty() || typed == password_mask() {
            return Ok(());
        }
        let id = Uuid::parse_str(&secret_ref.key).map_err(|_| {
            crate::localization::text("connections.credentials.invalid_sentinel_secret_id")
        })?;
        crate::sql_credentials::store_password(id, &typed)
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn store_password(
        &self,
        _profile: &ConnectionProfile,
        _cx: &gpui::App,
    ) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ConnectionEditor {
    fn toggle_sentinel(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.sentinel.enabled = checked;
        self.test_result = None;
        self.error = None;
        cx.notify();
    }

    fn toggle_prefer_replica(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.sentinel.prefer_replica_reads = checked;
        cx.notify();
    }

    /// The Sentinel section, shown for Redis-family engines.
    pub(super) fn render_sentinel_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let fields = &self.sentinel;
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .child(Self::render_checkbox(
                "ce-redis-sentinel",
                &crate::localization::text("connections.editor.sentinel_enable_label"),
                fields.enabled,
                false,
                cx,
                Self::toggle_sentinel,
            ));
        if fields.enabled {
            let master = Input::new(&fields.master_input).font_family(Theme::editor_font_family());
            let sentinels =
                Input::new(&fields.sentinels_input).font_family(Theme::editor_font_family());
            let password = Input::new(&fields.password_input)
                .font_family(Theme::editor_font_family())
                .mask_toggle();
            section = section
                .child(Self::labeled_field(
                    &crate::localization::text("connections.editor.sentinel_master_name_label"),
                    master,
                    cx,
                ))
                .child(Self::labeled_field(
                    &crate::localization::text("connections.editor.sentinel_hosts_label"),
                    sentinels,
                    cx,
                ))
                .child(Self::labeled_field(
                    &crate::localization::text("connections.editor.sentinel_password_label"),
                    password,
                    cx,
                ))
                .child(Self::render_checkbox(
                    "ce-redis-prefer-replica",
                    &crate::localization::text("connections.editor.sentinel_prefer_replica_label"),
                    fields.prefer_replica_reads,
                    false,
                    cx,
                    Self::toggle_prefer_replica,
                ))
                .child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "connections.editor.sentinel_behavior_note",
                        )),
                );
        }
        section.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keychain(key: &str) -> SecretRef {
        SecretRef {
            provider: SecretProvider::MacOsKeychain,
            key: key.into(),
        }
    }

    #[test]
    fn sentinel_switch_off_builds_no_config() {
        assert_eq!(build_config(false, "m", "h", None, true), Ok(None));
    }

    #[test]
    fn sentinel_fields_build_the_profile_config() {
        let config = build_config(true, " mymaster ", "a:1, b", None, true)
            .unwrap()
            .unwrap();
        assert_eq!(config.master_name, "mymaster");
        assert_eq!(config.sentinels, vec!["a:1".to_string(), "b".to_string()]);
        assert!(config.prefer_replica_reads);
        assert!(
            build_config(true, "", "a:1", None, false)
                .unwrap_err()
                .contains("master name")
        );
        assert!(
            build_config(true, "m", " ", None, false)
                .unwrap_err()
                .contains("at least one sentinel")
        );
    }

    #[test]
    fn sentinel_password_mask_keeps_the_saved_reference() {
        let saved = keychain("6f1d3b8e-0000-4000-8000-000000000001");
        assert_eq!(
            password_ref_for(&password_mask(), Some(&saved), true),
            Some(saved.clone())
        );
        assert_eq!(
            password_ref_for(&password_mask(), Some(&saved), false),
            Some(saved.clone())
        );
        // A newly typed password reuses the saved keyring slot on save …
        assert_eq!(password_ref_for("new", Some(&saved), true), Some(saved));
        // … and is used in memory, never stored, for Test Connection.
        assert_eq!(
            password_ref_for("new", None, false),
            Some(SecretRef {
                provider: SecretProvider::InMemory,
                key: "new".into()
            })
        );
        // A blank field removes the password.
        assert_eq!(password_ref_for("", Some(&keychain("x")), true), None);
    }

    #[test]
    fn sentinel_password_first_save_mints_a_keyring_reference() {
        let minted = password_ref_for("pw", None, true).unwrap();
        assert_eq!(minted.provider, SecretProvider::MacOsKeychain);
        assert!(Uuid::parse_str(&minted.key).is_ok());
        assert_ne!(
            minted.key, "pw",
            "the password itself never lands in the profile"
        );
    }

    #[test]
    fn sentinel_applies_to_redis_family_only() {
        assert!(applies_to(DatabaseType::Redis));
        assert!(applies_to(DatabaseType::Valkey));
        assert!(!applies_to(DatabaseType::Postgres));
    }
}
