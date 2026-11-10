//! Master-detail editor for clients / projects / environments entities.
//!
//! Port of `EntityListTabController.swift` + `EntityEditorView.swift`. The Swift
//! app exposes three tabs (Clients / Projects / Environments) backed by a single
//! `EntityListKind`; this view collapses them into one dialog with a kind
//! selector. Each entity has a name, a color, an icon path, and a `SafetyFeatures`
//! block (8 query-protection toggles). Edits persist to the same
//! `ConnectionStore` that holds connection profiles, so the server explorer and
//! connection manager see the new entities immediately.

use gpui::{
    AnyElement, ClickEvent, Context, Entity, EventEmitter, IntoElement, MouseButton, Render,
    ScrollHandle, Styled, Subscription, Window, div, prelude::*, px, relative,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariant, ButtonVariants},
    checkbox::Checkbox,
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    input::{Input, InputEvent, InputState},
    tab::{Tab, TabBar},
};
use uuid::Uuid;

use crate::theme::Theme;

use crate::connection::{
    ClientEntity, ConnectionStore, EnvironmentEntity, ProjectEntity, SafetyFeatures,
};
use crate::dialog::{Dialog, DialogFrame};
use crate::entity_editing::{
    is_valid_hex_color, list_label_default, normalized_name, parse_hex_color,
};
use crate::scrollbars::DraggableScrollbar;

/// Which of the 8 `SafetyFeatures` booleans a checkbox toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyField {
    IsProduction,
    OuterTransactionRollback,
    OnlyAllowSelect,
    ConfirmationDialog,
    RequireAiReview,
    AllowUnconstrainedUpdate,
    AllowUnconstrainedDelete,
    RequireTouchIdToConnect,
    RequireTouchIdForNonSelect,
}

impl SafetyField {
    pub fn all() -> &'static [SafetyField] {
        &[
            Self::IsProduction,
            Self::OuterTransactionRollback,
            Self::OnlyAllowSelect,
            Self::ConfirmationDialog,
            Self::RequireAiReview,
            Self::AllowUnconstrainedUpdate,
            Self::AllowUnconstrainedDelete,
            Self::RequireTouchIdToConnect,
            Self::RequireTouchIdForNonSelect,
        ]
    }

    pub fn label(self) -> String {
        crate::localization::text(match self {
            Self::IsProduction => "connections.entities.safety_production_label",
            Self::OuterTransactionRollback => "connections.entities.safety_rollback_label",
            Self::OnlyAllowSelect => "connections.entities.safety_select_only_label",
            Self::ConfirmationDialog => "connections.entities.safety_confirmation_label",
            Self::RequireAiReview => "connections.entities.safety_ai_review_label",
            Self::AllowUnconstrainedUpdate => "connections.entities.safety_unbounded_update_label",
            Self::AllowUnconstrainedDelete => "connections.entities.safety_unbounded_delete_label",
            Self::RequireTouchIdToConnect => "connections.entities.safety_touch_connect_label",
            Self::RequireTouchIdForNonSelect => {
                "connections.entities.safety_touch_non_select_label"
            }
        })
    }

    pub fn description(self) -> String {
        crate::localization::text(match self {
            Self::IsProduction => "connections.entities.safety_production_description",
            Self::OuterTransactionRollback => "connections.entities.safety_rollback_description",
            Self::OnlyAllowSelect => "connections.entities.safety_select_only_description",
            Self::ConfirmationDialog => "connections.entities.safety_confirmation_description",
            Self::RequireAiReview => "connections.entities.safety_ai_review_description",
            Self::AllowUnconstrainedUpdate => {
                "connections.entities.safety_unbounded_update_description"
            }
            Self::AllowUnconstrainedDelete => {
                "connections.entities.safety_unbounded_delete_description"
            }
            Self::RequireTouchIdToConnect => {
                "connections.entities.safety_touch_connect_description"
            }
            Self::RequireTouchIdForNonSelect => {
                "connections.entities.safety_touch_non_select_description"
            }
        })
    }
}

/// Flip one persisted safety toggle (XOR). Shared by the GPUI view and unit tests.
pub(crate) fn apply_safety_toggle(safety: &mut SafetyFeatures, field: SafetyField) {
    match field {
        SafetyField::IsProduction => safety.is_production ^= true,
        SafetyField::OuterTransactionRollback => {
            safety.use_outer_transaction_rollback_scope ^= true;
        }
        SafetyField::OnlyAllowSelect => safety.only_allow_select ^= true,
        SafetyField::ConfirmationDialog => safety.use_confirmation_dialog ^= true,
        SafetyField::RequireAiReview => safety.require_ai_review_for_data_changes ^= true,
        SafetyField::AllowUnconstrainedUpdate => safety.allow_unconstrained_update ^= true,
        SafetyField::AllowUnconstrainedDelete => safety.allow_unconstrained_delete ^= true,
        SafetyField::RequireTouchIdToConnect => safety.require_touch_id_to_connect ^= true,
        SafetyField::RequireTouchIdForNonSelect => safety.require_touch_id_for_non_select ^= true,
    }
}

/// Which entity collection is being edited. Mirrors `EntityListKind` in Swift
/// (all three kinds show a safety editor — `showsSafety` is always true).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Client,
    Project,
    Environment,
}

impl EntityKind {
    pub fn all() -> &'static [EntityKind] {
        &[Self::Client, Self::Project, Self::Environment]
    }

    pub fn label(self) -> String {
        crate::localization::text(match self {
            Self::Client => "connections.entities.clients_tab",
            Self::Project => "connections.entities.projects_tab",
            Self::Environment => "connections.entities.environments_tab",
        })
    }

    pub fn singular(self) -> String {
        crate::localization::text(match self {
            Self::Client => "connections.entities.client_singular",
            Self::Project => "connections.entities.project_singular",
            Self::Environment => "connections.entities.environment_singular",
        })
    }
}

/// Kind-agnostic snapshot of one stored entity, used to render the master list.
#[derive(Debug, Clone)]
struct EntityRow {
    id: Uuid,
    name: String,
    color_hex: Option<String>,
    image_path: Option<String>,
    safety: SafetyFeatures,
}

/// Unifies the three entity types into a `EntityRow` so `load_rows` can build
/// one `Vec<EntityRow>` regardless of the active kind.
trait IntoEntityRow {
    fn into_row(self) -> EntityRow;
}

impl IntoEntityRow for ClientEntity {
    fn into_row(self) -> EntityRow {
        EntityRow {
            id: self.id,
            name: self.name,
            color_hex: self.color_hex,
            image_path: self.image_path,
            safety: self.safety,
        }
    }
}

impl IntoEntityRow for ProjectEntity {
    fn into_row(self) -> EntityRow {
        EntityRow {
            id: self.id,
            name: self.name,
            color_hex: self.color_hex,
            image_path: self.image_path,
            safety: self.safety,
        }
    }
}

impl IntoEntityRow for EnvironmentEntity {
    fn into_row(self) -> EntityRow {
        EntityRow {
            id: self.id,
            name: self.name,
            color_hex: self.color_hex,
            image_path: self.image_path,
            safety: self.safety,
        }
    }
}

pub enum EntityManagementEvent {
    /// An entity was added, saved, or removed — the app should refresh the
    /// sidebar / connection manager so the new organization is visible.
    Changed,
    Cancelled,
}

impl EventEmitter<EntityManagementEvent> for EntityManagement {}

pub struct EntityManagement {
    kind: EntityKind,
    rows: Vec<EntityRow>,
    selected: Option<usize>,
    editing_id: Option<Uuid>,
    name: String,
    color_hex: String,
    image_path: String,
    safety: SafetyFeatures,
    status_message: String,
    list_scroll: ScrollHandle,
    form_scroll: ScrollHandle,
    name_input: Entity<InputState>,
    color_input: Entity<InputState>,
    image_input: Entity<InputState>,
    color_picker: Option<Entity<ColorPickerState>>,
    color_picker_subscription: Option<Subscription>,
    show_color_picker: bool,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

/// The editable entity extracted from the form fields (kind-agnostic). Replaces
/// a raw tuple now that the form carries five fields (incl. the icon path).
struct FormEntity {
    id: Uuid,
    name: String,
    color_hex: Option<String>,
    image_path: Option<String>,
    safety: SafetyFeatures,
}

impl EntityManagement {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.entities.name_placeholder",
            ))
        });
        let color_input = cx.new(|cx| InputState::new(window, cx).placeholder("#rrggbb"));
        let image_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.entities.icon_path_placeholder",
            ))
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &name_input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.name = input.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &color_input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.color_hex = input.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &image_input,
                window,
                |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.image_path = input.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ),
        ];
        let mut view = Self {
            kind: EntityKind::Client,
            rows: Vec::new(),
            selected: None,
            editing_id: None,
            name: String::new(),
            color_hex: String::new(),
            image_path: String::new(),
            safety: SafetyFeatures::default(),
            status_message: String::new(),
            list_scroll: ScrollHandle::new(),
            form_scroll: ScrollHandle::new(),
            name_input,
            color_input,
            image_input,
            color_picker: None,
            color_picker_subscription: None,
            show_color_picker: false,
            frame: DialogFrame::new(Theme::scaled(760.0), Theme::scaled(520.0)),
            _subscriptions: subscriptions,
        };
        view.load_rows();
        view.select_first();
        view.load_selected_into_form(window, cx);
        view
    }

    /// Re-reads the active collection from the store into `rows`.
    fn load_rows(&mut self) {
        let store = ConnectionStore::global();
        self.rows = match self.kind {
            EntityKind::Client => store
                .clients()
                .into_iter()
                .map(IntoEntityRow::into_row)
                .collect(),
            EntityKind::Project => store
                .projects()
                .into_iter()
                .map(IntoEntityRow::into_row)
                .collect(),
            EntityKind::Environment => store
                .environments()
                .into_iter()
                .map(IntoEntityRow::into_row)
                .collect(),
        };
    }

    fn select_first(&mut self) {
        self.selected = if self.rows.is_empty() { None } else { Some(0) };
        self.editing_id = self.selected.and_then(|i| self.rows.get(i).map(|r| r.id));
    }

    /// Loads the selected row's name/color/safety into the detail form fields.
    fn load_selected_into_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = self.selected.and_then(|i| self.rows.get(i)).cloned();
        if let Some(row) = row {
            self.name = row.name;
            self.color_hex = row.color_hex.unwrap_or_default();
            self.image_path = row.image_path.unwrap_or_default();
            self.safety = row.safety;
            self.editing_id = Some(row.id);
        } else {
            self.name.clear();
            self.color_hex.clear();
            self.image_path.clear();
            self.safety = SafetyFeatures::default();
            self.editing_id = None;
        }
        self.sync_form_inputs(window, cx);
    }

    /// Pushes the canonical form strings into the input entities (after a
    /// selection change or any other programmatic form update).
    fn sync_form_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fields = [
            (self.name_input.clone(), self.name.clone()),
            (self.color_input.clone(), self.color_hex.clone()),
            (self.image_input.clone(), self.image_path.clone()),
        ];
        for (input, value) in fields {
            input.update(cx, |state, cx| {
                if state.value().as_ref() != value.as_str() {
                    state.set_value(value, window, cx);
                }
            });
        }
    }

    fn select(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx < self.rows.len() {
            self.selected = Some(idx);
            self.editing_id = Some(self.rows[idx].id);
            self.load_selected_into_form(window, cx);
            cx.notify();
        }
    }

    fn set_kind(&mut self, kind: EntityKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.kind == kind {
            return;
        }
        self.kind = kind;
        self.load_rows();
        self.select_first();
        self.load_selected_into_form(window, cx);
        self.status_message.clear();
        cx.notify();
    }

    /// Builds a typed entity from the current form state. Returns `None` when
    /// the name is blank (Save is disabled in that case, but this is the guard).
    fn form_entity(&self) -> Option<FormEntity> {
        let name = normalized_name(&self.name)?;
        let raw_color = self.color_hex.trim().to_string();
        let color_hex = if raw_color.is_empty() {
            None
        } else if is_valid_hex_color(&raw_color) {
            Some(raw_color)
        } else {
            None
        };
        let raw_icon = self.image_path.trim().to_string();
        let image_path = if raw_icon.is_empty() {
            None
        } else {
            Some(raw_icon)
        };
        Some(FormEntity {
            id: self.editing_id.unwrap_or_else(Uuid::new_v4),
            name,
            color_hex,
            image_path,
            safety: self.safety.clone(),
        })
    }

    fn handle_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = Uuid::new_v4();
        let key = match self.kind {
            EntityKind::Client => "connections.entities.new_client",
            EntityKind::Project => "connections.entities.new_project",
            EntityKind::Environment => "connections.entities.new_environment",
        };
        let name = crate::localization::text(key);
        let entity = FormEntity {
            id,
            name,
            color_hex: None,
            image_path: None,
            safety: SafetyFeatures::default(),
        };
        self.upsert_entity(entity, cx);
        self.load_rows();
        if let Some(idx) = self.rows.iter().position(|r| r.id == id) {
            self.select(idx, window, cx);
        }
        self.status_message = crate::localization::text(match self.kind {
            EntityKind::Client => "connections.entities.added_client",
            EntityKind::Project => "connections.entities.added_project",
            EntityKind::Environment => "connections.entities.added_environment",
        });
        // Focus the name field for immediate renaming.
        self.name.clear();
        self.name_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn handle_save(&mut self, cx: &mut Context<Self>) {
        let Some(entity) = self.form_entity() else {
            self.status_message = crate::localization::text("connections.entities.name_required");
            cx.notify();
            return;
        };
        let saved_name = entity.name.clone();
        self.upsert_entity(entity, cx);
        self.load_rows();
        if let Some(idx) = self
            .rows
            .iter()
            .position(|r| r.id == self.editing_id.unwrap_or_else(Uuid::new_v4))
        {
            self.selected = Some(idx);
        }
        self.status_message = crate::localization::format(
            "connections.entities.saved_named",
            &[("entity_name", &saved_name)],
        );
        cx.emit(EntityManagementEvent::Changed);
        cx.notify();
    }

    fn handle_remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.editing_id else {
            return;
        };
        let store = ConnectionStore::global();
        match self.kind {
            EntityKind::Client => store.remove_client(id),
            EntityKind::Project => store.remove_project(id),
            EntityKind::Environment => store.remove_environment(id),
        }
        self.load_rows();
        self.select_first();
        self.load_selected_into_form(window, cx);
        self.status_message = crate::localization::text("connections.entities.removed");
        cx.emit(EntityManagementEvent::Changed);
        cx.notify();
    }

    fn handle_close(&mut self, cx: &mut Context<Self>) {
        cx.emit(EntityManagementEvent::Cancelled);
    }

    fn toggle_color_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_color_picker = !self.show_color_picker;
        if self.show_color_picker {
            let color = parse_hex_color(&self.color_hex)
                .map(|(r, g, b, _)| rgb_to_hsla(r, g, b))
                .unwrap_or_else(|| rgb_to_hsla(128, 128, 128));
            let picker = cx.new(|cx| ColorPickerState::new(window, cx).default_value(color));
            self.color_picker_subscription = Some(cx.subscribe_in(
                &picker,
                window,
                |this: &mut Self, _picker, event: &ColorPickerEvent, window, cx| {
                    let ColorPickerEvent::Change(color) = event;
                    if let Some(color) = color {
                        let hex = hsla_to_hex_string(*color);
                        if this.color_hex != hex {
                            this.color_hex = hex.clone();
                            this.color_input
                                .update(cx, |state, cx| state.set_value(hex, window, cx));
                            cx.notify();
                        }
                    }
                },
            ));
            self.color_picker = Some(picker);
        } else {
            self.color_picker = None;
            self.color_picker_subscription = None;
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn handle_browse_icon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let file = rfd::FileDialog::new()
            .add_filter(
                crate::localization::text("connections.entities.image_file_filter"),
                &["png", "jpg", "jpeg", "gif", "svg", "ico", "bmp", "webp"],
            )
            .set_title(crate::localization::text(
                "connections.entities.select_icon_title",
            ))
            .pick_file();
        if let Some(path) = file {
            self.image_path = path.to_string_lossy().to_string();
            self.sync_form_inputs(window, cx);
            cx.notify();
        }
    }

    /// Browser build: there is no native file dialog; report it in the pane's
    /// status line.
    #[cfg(target_arch = "wasm32")]
    fn handle_browse_icon(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.status_message =
            crate::localization::text("connections.entities.browser_icon_picker_unavailable");
        cx.notify();
    }

    /// Dispatches an upsert to the active collection.
    fn upsert_entity(&self, entity: FormEntity, _cx: &mut Context<Self>) {
        let FormEntity {
            id,
            name,
            color_hex,
            image_path,
            safety,
        } = entity;
        let store = ConnectionStore::global();
        match self.kind {
            EntityKind::Client => store.upsert_client(ClientEntity {
                id,
                name,
                color_hex,
                image_path,
                safety,
            }),
            EntityKind::Project => store.upsert_project(ProjectEntity {
                id,
                name,
                color_hex,
                image_path,
                safety,
            }),
            EntityKind::Environment => store.upsert_environment(EnvironmentEntity {
                id,
                name,
                color_hex,
                image_path,
                safety,
            }),
        }
    }

    fn toggle_safety(&mut self, field: SafetyField, cx: &mut Context<Self>) {
        apply_safety_toggle(&mut self.safety, field);
        cx.notify();
    }

    fn can_save(&self) -> bool {
        normalized_name(&self.name).is_some()
    }

    // ---- Render helpers (mirror connection_management.rs) ----

    fn render_section(label: &str, children: Vec<AnyElement>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(6.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(Theme::text())
                    .child(label.to_string()),
            )
            .children(children)
            .into_any_element()
    }

    fn labeled_field(label: &str, field: impl IntoElement) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(4.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(label.to_string()),
            )
            .child(field)
            .into_any_element()
    }

    /// The Clients / Projects / Environments tab strip at the top of the
    /// dialog (gpui-component `TabBar`).
    fn render_kind_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = EntityKind::all()
            .iter()
            .position(|k| *k == self.kind)
            .unwrap_or(0);
        let this = cx.entity();
        div()
            .px(px(Theme::scaled(12.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                TabBar::new("em-kind-tabs")
                    .underline()
                    .selected_index(selected)
                    .on_click(move |index: &usize, window, app| {
                        let index = *index;
                        this.update(app, |view, cx| {
                            if let Some(kind) = EntityKind::all().get(index) {
                                view.set_kind(*kind, window, cx);
                            }
                        });
                    })
                    .children(
                        EntityKind::all()
                            .iter()
                            .map(|kind| Tab::new().label(kind.label())),
                    ),
            )
            .into_any_element()
    }

    fn render_safety_checkbox(
        index: usize,
        field: SafetyField,
        checked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity();
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(2.0)))
            .child(
                Checkbox::new(("em_safety", index))
                    .checked(checked)
                    .label(field.label())
                    .tab_stop(true)
                    .on_click(move |_checked: &bool, _window, app| {
                        this.update(app, |this, cx| this.toggle_safety(field, cx));
                    }),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text_dim())
                    .child(field.description()),
            )
            .into_any_element()
    }

    /// A gpui-component `Button` whose click runs an entity method.
    fn kit_button(
        id: &'static str,
        label: String,
        variant: ButtonVariant,
        enabled: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let this = cx.entity();
        Button::new(id)
            .label(label)
            .with_variant(variant)
            .disabled(!enabled)
            .on_click(move |_: &ClickEvent, window, app| {
                this.update(app, |view, cx| on_click(view, window, cx));
            })
            .into_any_element()
    }

    /// An always-visible vertical scrollbar for `handle`'s scroll container.
    /// When the content fits (nothing to scroll) the bar renders in a
    /// disabled state: full-height dim thumb, no interaction. Pressing the
    /// track jumps to the corresponding position; holding drags.
    fn vertical_scrollbar(id: &'static str, handle: &ScrollHandle) -> AnyElement {
        let s = Theme::scaled;
        let viewport = handle.bounds().size.height;
        let max_offset = handle.max_offset().y;
        let scrollable = max_offset > px(1.0) && viewport > px(0.0);

        let (thumb_top, thumb_height) = if scrollable {
            let content = viewport + max_offset;
            let height = (viewport / content).clamp(0.08, 1.0);
            let progress = (-handle.offset().y / max_offset).clamp(0.0, 1.0);
            (progress * (1.0 - height), height)
        } else {
            (0.0, 1.0)
        };

        let track = div()
            .id(id)
            .w(px(s(10.0)))
            .h_full()
            .flex_none()
            .relative()
            .bg(Theme::panel_bg())
            .border_l_1()
            .border_color(Theme::border())
            .child(
                div()
                    .absolute()
                    .top(relative(thumb_top))
                    .left(px(s(2.0)))
                    .right(px(s(2.0)))
                    .h(relative(thumb_height))
                    .rounded(px(s(3.0)))
                    .bg(if scrollable {
                        Theme::text_dim().opacity(0.5)
                    } else {
                        Theme::text_dim().opacity(0.15)
                    }),
            );

        if !scrollable {
            return track.into_any_element();
        }

        track
            .cursor(gpui::CursorStyle::PointingHand)
            // Press jumps there; holding drags (tracked by the dialog root's
            // `scrollbar_drag_area`).
            .scrollbar_track(handle, gpui::Axis::Vertical)
            .into_any_element()
    }

    /// A small color preview swatch rendered from the current hex input.
    fn color_swatch(hex: &str) -> AnyElement {
        let s = Theme::scaled;
        let (r, g, b, _) = parse_hex_color(hex).unwrap_or((128, 128, 128, 255));
        div()
            .w(px(s(20.0)))
            .h(px(s(20.0)))
            .border_1()
            .border_color(Theme::border())
            .rounded(px(s(3.0)))
            .bg(rgb_to_hsla(r, g, b))
            .into_any_element()
    }
}

/// gpui `Hsla` -> lowercase `#rrggbb` (the canonical form the color field and
/// store use). One shared implementation lives in `crate::theme`.
use crate::theme::hsla_to_hex_string;

/// sRGB (0–255) -> gpui `Hsla`. Hue normalized to 0..1 for `gpui::hsla`.
fn rgb_to_hsla(r: u8, g: u8, b: u8) -> gpui::Hsla {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let l = (max + min) / 2.0;
    let s = if delta == 0.0 {
        0.0
    } else {
        delta / (1.0 - (2.0 * l - 1.0).abs())
    };
    let h = if delta == 0.0 {
        0.0
    } else if max == r {
        let h6 = ((g - b) / delta) % 6.0;
        60.0 * if h6 < 0.0 { h6 + 6.0 } else { h6 }
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    gpui::hsla(h / 360.0, s, l, 1.0)
}

impl Render for EntityManagement {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;

        let kind = self.kind;
        let selected = self.selected;
        let safety = self.safety.clone();
        let can_save = self.can_save();
        let status_message = self.status_message.clone();
        let color_text = self.color_hex.clone();

        let name_input = Input::new(&self.name_input);
        let color_input = Input::new(&self.color_input);
        let image_input = Input::new(&self.image_input);

        // Color picker entity (if toggled open).
        let color_picker_entity = self.color_picker.clone();

        // Master list rows.
        let rows: Vec<AnyElement> = self
            .rows
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                let is_selected = selected == Some(idx);
                let label = list_label_default(&row.name);
                div()
                    .id(("em_row", idx))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .px(px(s(8.0)))
                    .h(px(s(26.0)))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .bg(if is_selected {
                        Theme::accent().opacity(0.3)
                    } else {
                        Theme::sidebar_bg()
                    })
                    .hover(|st| st.bg(Theme::border()))
                    .text_size(px(Theme::font_size()))
                    .text_color(if is_selected {
                        Theme::text()
                    } else {
                        Theme::text_dim()
                    })
                    .child(
                        // Color dot (if any).
                        row.color_hex
                            .as_deref()
                            .and_then(parse_hex_color)
                            .map(|(r, g, b, _)| {
                                div()
                                    .w(px(s(10.0)))
                                    .h(px(s(10.0)))
                                    .rounded(px(s(5.0)))
                                    .bg(rgb_to_hsla(r, g, b))
                                    .into_any_element()
                            })
                            .unwrap_or_else(|| div().w(px(s(10.0))).into_any_element()),
                    )
                    .child(label)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| this.select(idx, window, cx)),
                    )
                    .into_any_element()
            })
            .collect();

        // Safety checkboxes.
        let safety_section = Self::render_section(
            &crate::localization::text("connections.entities.query_protections_section"),
            SafetyField::all()
                .iter()
                .enumerate()
                .map(|(i, field)| {
                    let checked = match *field {
                        SafetyField::IsProduction => safety.is_production,
                        SafetyField::OuterTransactionRollback => {
                            safety.use_outer_transaction_rollback_scope
                        }
                        SafetyField::OnlyAllowSelect => safety.only_allow_select,
                        SafetyField::ConfirmationDialog => safety.use_confirmation_dialog,
                        SafetyField::RequireAiReview => safety.require_ai_review_for_data_changes,
                        SafetyField::AllowUnconstrainedUpdate => safety.allow_unconstrained_update,
                        SafetyField::AllowUnconstrainedDelete => safety.allow_unconstrained_delete,
                        SafetyField::RequireTouchIdToConnect => safety.require_touch_id_to_connect,
                        SafetyField::RequireTouchIdForNonSelect => {
                            safety.require_touch_id_for_non_select
                        }
                    };
                    Self::render_safety_checkbox(i, *field, checked, cx)
                })
                .collect(),
        );

        // ---- Left pane: master list for the active tab + add/remove ----
        let has_selection = self.selected.is_some();
        let left_pane = div()
            .w(px(s(220.0)))
            .h_full()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(Theme::sidebar_bg())
            .border_r_1()
            .border_color(Theme::border())
            .child(
                div()
                    .id("em_list")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.list_scroll)
                    .on_scroll(cx.listener(|_, _, _, cx| cx.notify()))
                    .child(if rows.is_empty() {
                        div()
                            .p(px(s(12.0)))
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(match kind {
                                EntityKind::Client => "connections.entities.empty_clients",
                                EntityKind::Project => "connections.entities.empty_projects",
                                EntityKind::Environment => {
                                    "connections.entities.empty_environments"
                                }
                            }))
                            .into_any_element()
                    } else {
                        div().flex().flex_col().children(rows).into_any_element()
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(6.0)))
                    .p(px(s(8.0)))
                    .border_t_1()
                    .border_color(Theme::border())
                    .child(Self::kit_button(
                        "em_add",
                        crate::localization::text("connections.entities.add_button"),
                        ButtonVariant::Secondary,
                        true,
                        cx,
                        Self::handle_add,
                    ))
                    .child(Self::kit_button(
                        "em_remove",
                        crate::localization::text("connections.entities.remove_button"),
                        ButtonVariant::Danger,
                        has_selection,
                        cx,
                        Self::handle_remove,
                    )),
            );

        // ---- Right pane: detail editor ----
        let color_field = div()
            .flex()
            .flex_row()
            .gap(px(s(8.0)))
            .items_center()
            .child(div().flex_1().child(color_input))
            .child(Self::color_swatch(&color_text))
            .child(Self::kit_button(
                "em_color_pick",
                crate::localization::text(if self.show_color_picker {
                    "connections.entities.close_picker_button"
                } else {
                    "connections.entities.pick_color_button"
                }),
                ButtonVariant::Secondary,
                true,
                cx,
                Self::toggle_color_picker,
            ))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(
                        if color_text.is_empty() || is_valid_hex_color(&color_text) {
                            Theme::text_dim()
                        } else {
                            Theme::error()
                        },
                    )
                    .child(crate::localization::text(if color_text.is_empty() {
                        "connections.entities.color_optional"
                    } else if is_valid_hex_color(&color_text) {
                        "connections.entities.color_valid"
                    } else {
                        "connections.entities.color_invalid"
                    })),
            );

        // Inline color picker (toggled by the Pick button).
        let color_picker_child: Option<AnyElement> = color_picker_entity
            .as_ref()
            .map(|picker| div().child(ColorPicker::new(picker)).into_any_element());

        let icon_field = div()
            .flex()
            .flex_row()
            .gap(px(s(8.0)))
            .items_center()
            .child(div().flex_1().child(image_input))
            .child(Self::kit_button(
                "em_icon_browse",
                crate::localization::text("connections.entities.browse_icon_button"),
                ButtonVariant::Secondary,
                true,
                cx,
                Self::handle_browse_icon,
            ));

        let form_body = div()
            .id("em_form")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.form_scroll)
            .on_scroll(cx.listener(|_, _, _, cx| cx.notify()))
            .p(px(s(16.0)))
            .flex()
            .flex_col()
            .gap(px(s(16.0)))
            .child(Self::labeled_field(
                &crate::localization::text("connections.entities.name_label"),
                name_input,
            ))
            .child(Self::labeled_field(
                &crate::localization::text("connections.entities.color_label"),
                color_field,
            ))
            .children(color_picker_child)
            .child(Self::labeled_field(
                &crate::localization::text("connections.entities.icon_label"),
                icon_field,
            ))
            .child(safety_section);

        // The details form scrolls next to an always-visible scrollbar; the
        // bar renders disabled (dim, inert) when the form fits its viewport.
        let right_pane = div()
            .flex_1()
            .h_full()
            .min_h(px(0.0))
            .flex()
            .flex_row()
            .child(form_body)
            .child(Self::vertical_scrollbar(
                "em_form_scrollbar",
                &self.form_scroll,
            ));

        let status_line = div()
            .text_size(px(Theme::font_size()))
            .text_color(Theme::text_dim())
            .child(status_message);

        Dialog::new("em-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text(
                "connections.entities.dialog_title",
            ))
            .bare_body()
            .on_close(|this, _window, cx| this.handle_close(cx))
            // Clients / Projects / Environments tabs — every section of the
            // dialog lives under one of these.
            .child(self.render_kind_tabs(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_row()
                    .child(left_pane)
                    .child(right_pane),
            )
            .leading_button(status_line)
            .button(Self::kit_button(
                "em_save",
                crate::localization::text("connections.entities.save_button"),
                ButtonVariant::Primary,
                can_save,
                cx,
                |this, _window, cx| this.handle_save(cx),
            ))
            .build(self, window, cx)
            // Keeps a scrollbar drag tracking once the pointer leaves the
            // narrow track (the details-form scrollbar).
            .scrollbar_drag_area()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_kinds_have_distinct_labels_and_singulars() {
        let labels: Vec<String> = EntityKind::all().iter().map(|k| k.label()).collect();
        let singulars: Vec<String> = EntityKind::all().iter().map(|k| k.singular()).collect();
        for text in labels.iter().chain(singulars.iter()) {
            assert!(!text.is_empty());
        }
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), EntityKind::all().len());
    }

    #[test]
    fn safety_fields_have_distinct_labels() {
        let labels: Vec<String> = SafetyField::all().iter().map(|f| f.label()).collect();
        assert!(!labels.is_empty());
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len());
    }

    fn serde_field_name(field: SafetyField) -> &'static str {
        match field {
            SafetyField::IsProduction => "is_production",
            SafetyField::OuterTransactionRollback => "use_outer_transaction_rollback_scope",
            SafetyField::OnlyAllowSelect => "only_allow_select",
            SafetyField::ConfirmationDialog => "use_confirmation_dialog",
            SafetyField::RequireAiReview => "require_ai_review_for_data_changes",
            SafetyField::AllowUnconstrainedUpdate => "allow_unconstrained_update",
            SafetyField::AllowUnconstrainedDelete => "allow_unconstrained_delete",
            SafetyField::RequireTouchIdToConnect => "require_touch_id_to_connect",
            SafetyField::RequireTouchIdForNonSelect => "require_touch_id_for_non_select",
        }
    }

    fn count_true_fields(value: &serde_json::Value) -> usize {
        value
            .as_object()
            .expect("object")
            .values()
            .filter(|v| matches!(v, serde_json::Value::Bool(true)))
            .count()
    }

    #[test]
    fn safety_toggle_flips_one_field_and_round_trips() {
        for field in SafetyField::all() {
            let mut safety = SafetyFeatures::default();
            apply_safety_toggle(&mut safety, *field);
            let value = serde_json::to_value(&safety).expect("serialize");
            assert_eq!(count_true_fields(&value), 1);
            assert_eq!(
                value.get(serde_field_name(*field)),
                Some(&serde_json::Value::Bool(true))
            );
            apply_safety_toggle(&mut safety, *field);
            let again = serde_json::to_value(&safety).expect("serialize");
            assert_eq!(count_true_fields(&again), 0);
        }
    }
}
