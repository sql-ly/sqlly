//! Per-tab state and rendering for the Parameters strip: a flat band above
//! the results with one input per `-- sqlly param:` declaration and per
//! `:name` / `?` / `$1` placeholder written in the SQL. The pure grammar
//! lives in [`crate::query_params`]; this module owns the stateful inputs
//! and the sync-with-buffer discipline (rows are reused by name+type so
//! typing elsewhere never wipes entered values).

use crate::tooltip::AppTooltip;
use gpui::{
    AnyElement, Context, Entity, IntoElement, ParentElement, Styled, Window, div, prelude::*, px,
};
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};

use crate::param_history::MAX_VALUES_PER_NAME;
use crate::query_params::{ParamSpec, ParamType, validate_value};
use crate::theme::Theme;

/// Whether existing strip rows already match the buffer's declared parameters
/// (same count and equal specs in order).
pub(crate) fn param_rows_match_specs(rows: &[ParamRow], specs: &[ParamSpec]) -> bool {
    rows.len() == specs.len() && rows.iter().zip(specs).all(|(row, spec)| row.spec == *spec)
}

/// Reconcile declared parameter specs with existing strip rows, preserving
/// entered values when the name (case-insensitive) and type still match. A
/// value typed for an undeclared placeholder carries over when a directive
/// later gives it a type.
pub(crate) fn merge_param_row_specs(
    old_rows: &[ParamRow],
    specs: &[ParamSpec],
) -> Vec<(ParamSpec, String)> {
    let mut consumed = vec![false; old_rows.len()];
    specs
        .iter()
        .map(|spec| {
            if let Some((idx, row)) = old_rows.iter().enumerate().find(|(i, row)| {
                !consumed[*i]
                    && row.spec.name.eq_ignore_ascii_case(&spec.name)
                    && (row.spec.ty == spec.ty || row.spec.ty == ParamType::Auto)
            }) {
                consumed[idx] = true;
                (spec.clone(), row.value.clone())
            } else {
                (spec.clone(), String::new())
            }
        })
        .collect()
}

/// One parameter row: the spec plus its stateful input and canonical value.
pub struct ParamRow {
    pub spec: ParamSpec,
    pub input: Option<Entity<InputState>>,
    pub value: String,
    _subscription: Option<gpui::Subscription>,
}

#[derive(Default)]
pub struct ParamStripState {
    pub rows: Vec<ParamRow>,
    /// Set when Run blocked on this parameter; the next strip render focuses
    /// its input (renders have the `Window` that event handlers lack).
    pub pending_focus: Option<String>,
    /// The specs last read from the buffer, keyed by what they were read
    /// from, so a render that changed nothing skips the scan.
    spec_cache: Option<SpecCache>,
}

struct SpecCache {
    text_hash: u64,
    text_len: usize,
    db: sqlly_lang::sqlmodel::DatabaseType,
    detect_placeholders: bool,
    specs: Vec<ParamSpec>,
}

impl ParamStripState {
    /// The strip's specs for `text` (see [`crate::query_params::strip_specs`]),
    /// recomputed only when the text, engine, or setting changed.
    pub(crate) fn specs_for(
        &mut self,
        db: sqlly_lang::sqlmodel::DatabaseType,
        text: &str,
        detect_placeholders: bool,
    ) -> Vec<ParamSpec> {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let text_hash = hasher.finish();
        if let Some(cache) = &self.spec_cache
            && cache.text_hash == text_hash
            && cache.text_len == text.len()
            && cache.db == db
            && cache.detect_placeholders == detect_placeholders
        {
            return cache.specs.clone();
        }
        let specs = crate::query_params::strip_specs(db, text, detect_placeholders);
        self.spec_cache = Some(SpecCache {
            text_hash,
            text_len: text.len(),
            db,
            detect_placeholders,
            specs: specs.clone(),
        });
        specs
    }
}

impl ParamStripState {
    /// The (name, value) pairs the substitution consumes.
    pub fn values(&self) -> Vec<(String, String)> {
        self.rows
            .iter()
            .map(|row| (row.spec.name.clone(), row.value.clone()))
            .collect()
    }

    /// Every row's spec and its current value, for carrying the strip to
    /// another window (Move Tab to Window). The input's live text wins over
    /// the canonical `value`: a value typed into a still-focused input can
    /// race the move, the same way it races F5.
    pub(crate) fn snapshot(&self, cx: &gpui::App) -> Vec<(ParamSpec, String)> {
        self.rows
            .iter()
            .map(|row| {
                let value = row
                    .input
                    .as_ref()
                    .map(|input| input.read(cx).value().to_string())
                    .unwrap_or_else(|| row.value.clone());
                (row.spec.clone(), value)
            })
            .collect()
    }

    /// A strip seeded from [`Self::snapshot`] in another window. The rows
    /// have no inputs yet (building one needs a `Window`); the next render's
    /// sync gives each an input prefilled with its value, and values survive
    /// the usual reconcile with the buffer's declarations (same name and
    /// type).
    pub(crate) fn seeded(values: Vec<(ParamSpec, String)>) -> Self {
        Self {
            rows: values
                .into_iter()
                .map(|(spec, value)| ParamRow {
                    spec,
                    input: None,
                    value,
                    _subscription: None,
                })
                .collect(),
            pending_focus: None,
            spec_cache: None,
        }
    }

    /// The non-empty values to save with the tab in the workspace session.
    /// Values come from [`Self::snapshot`], so text still in a focused input
    /// is included.
    pub(crate) fn persisted(
        &self,
        cx: &gpui::App,
    ) -> Vec<crate::workspace_session::SavedParamValue> {
        saved_param_values(self.snapshot(cx))
    }

    /// A strip restored from a saved session: rows without inputs (like a tab
    /// move), which the next render reconciles with the buffer's
    /// declarations by name and type. Values of a type this build doesn't
    /// know are dropped.
    pub(crate) fn from_persisted(saved: &[crate::workspace_session::SavedParamValue]) -> Self {
        Self::seeded(restored_param_values(saved))
    }
}

/// Session form of strip values: empty values are not saved.
fn saved_param_values(
    values: Vec<(ParamSpec, String)>,
) -> Vec<crate::workspace_session::SavedParamValue> {
    values
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(spec, value)| crate::workspace_session::SavedParamValue {
            name: spec.name,
            ty: spec.ty.label().to_string(),
            value,
        })
        .collect()
}

/// Strip values from their session form, skipping unknown types.
fn restored_param_values(
    saved: &[crate::workspace_session::SavedParamValue],
) -> Vec<(ParamSpec, String)> {
    saved
        .iter()
        .filter_map(|param| {
            let ty = ParamType::parse(&param.ty).ok()?;
            Some((
                ParamSpec {
                    name: param.name.clone(),
                    ty,
                    default: None,
                },
                param.value.clone(),
            ))
        })
        .collect()
}

/// Placeholder text for a parameter's input, by type.
fn param_placeholder(ty: ParamType) -> String {
    let key = match ty {
        ParamType::Date => "query.params_strip.placeholder_date",
        ParamType::DateTime => "query.params_strip.placeholder_datetime",
        ParamType::Time => "query.params_strip.placeholder_time",
        ParamType::Bool => "query.params_strip.placeholder_bool",
        ParamType::Uuid => return "00000000-0000-0000-0000-000000000000".to_string(),
        ParamType::Json => "query.params_strip.placeholder_json",
        ParamType::Binary => "query.params_strip.placeholder_binary",
        ParamType::Auto => "query.params_strip.placeholder_auto",
        _ => "query.params_strip.placeholder_value",
    };
    crate::localization::text(key)
}

impl crate::app::AppView {
    /// Reconcile the strip rows with the buffer's current declarations,
    /// keeping entered values for parameters that still exist with the same
    /// type. A row without an input (seeded by a tab move) gets one here,
    /// prefilled with its value.
    fn sync_param_rows(&mut self, tab_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        let tab_id = tab.id.clone();
        let editor = tab.editor.clone();
        let detect = self.detect_query_placeholders();
        let editor = editor.read(cx);
        let specs = self.tabs[tab_idx].query_params.specs_for(
            editor.database_type(),
            editor.text(),
            detect,
        );
        let rows = &self.tabs[tab_idx].query_params.rows;
        if param_rows_match_specs(rows, &specs) && rows.iter().all(|row| row.input.is_some()) {
            return;
        }
        let mut old_rows = std::mem::take(&mut self.tabs[tab_idx].query_params.rows);
        let merged = merge_param_row_specs(&old_rows, &specs);
        let mut rows = Vec::with_capacity(merged.len());
        for (spec, value) in merged {
            let reused = old_rows
                .iter()
                .position(|row| {
                    row.spec.name.eq_ignore_ascii_case(&spec.name) && row.spec.ty == spec.ty
                })
                .map(|idx| old_rows.remove(idx));
            if let Some(mut old) = reused
                && old.input.is_some()
            {
                rows.push(ParamRow {
                    spec,
                    input: old.input.take(),
                    value,
                    _subscription: old._subscription.take(),
                });
                continue;
            }
            let placeholder = param_placeholder(spec.ty);
            let initial = value.clone();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(initial)
            });
            let name = spec.name.clone();
            let owner = tab_id.clone();
            // Route edits by the owning tab's id, never the active index: a
            // split shows two tabs' strips, and typing into the inactive
            // pane's strip must not overwrite the active tab's value.
            let subscription = cx.subscribe_in(
                &input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = state.read(cx).value().to_string();
                        if let Some(index) = this.tabs.iter().position(|tab| tab.id == owner)
                            && let Some(row) = this.tabs[index]
                                .query_params
                                .rows
                                .iter_mut()
                                .find(|row| row.spec.name.eq_ignore_ascii_case(&name))
                        {
                            row.value = value;
                            // Entered values persist with the tab.
                            this.schedule_session_autosave(cx);
                        }
                        cx.notify();
                    }
                },
            );
            rows.push(ParamRow {
                spec,
                input: Some(input),
                value,
                _subscription: Some(subscription),
            });
        }
        self.tabs[tab_idx].query_params.rows = rows;
    }

    /// The parameter strip, or `None` when the buffer declares no parameters
    /// (or Procedure mode owns the band).
    pub(crate) fn render_query_params_strip(
        &mut self,
        tab_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let s = Theme::scaled;
        if self
            .tabs
            .get(tab_idx)
            .is_none_or(|tab| tab.proc_mode.enabled)
        {
            return None;
        }
        self.sync_param_rows(tab_idx, window, cx);
        if let Some(name) = self
            .tabs
            .get_mut(tab_idx)
            .and_then(|tab| tab.query_params.pending_focus.take())
        {
            let input = self.tabs.get(tab_idx).and_then(|tab| {
                tab.query_params
                    .rows
                    .iter()
                    .find(|row| row.spec.name.eq_ignore_ascii_case(&name))
                    .and_then(|row| row.input.clone())
            });
            if let Some(input) = input {
                input.update(cx, |state, cx| state.focus(window, cx));
            }
        }
        let tab = self.tabs.get(tab_idx)?;
        if tab.query_params.rows.is_empty() {
            return None;
        }
        let suggestion_sets: Vec<Vec<String>> = tab
            .query_params
            .rows
            .iter()
            .map(|row| self.param_history.suggestions(&row.spec.name).to_vec())
            .collect();

        let mut rows_el = div().flex().flex_col();
        for (idx, row) in tab.query_params.rows.iter().enumerate() {
            let effective = if row.value.trim().is_empty() {
                row.spec.default.as_deref()
            } else {
                Some(row.value.trim())
            };
            let status: Option<(String, bool)> = match effective {
                None => Some((
                    crate::localization::text("query.params_strip.required_status"),
                    true,
                )),
                Some(value) => match validate_value(row.spec.ty, value) {
                    Err(err) => Some((err, true)),
                    Ok(()) if row.value.trim().is_empty() => Some((
                        crate::localization::format(
                            "query.params_strip.default_status",
                            &[("value", value)],
                        ),
                        false,
                    )),
                    Ok(()) => None,
                },
            };
            let mut row_el = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .px(px(s(12.0)))
                .py(px(s(2.0)))
                .child(
                    div()
                        .w(px(s(170.0)))
                        .flex_shrink_0()
                        .truncate()
                        .font_family(Theme::editor_font_family())
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text())
                        .child(row.spec.name.clone()),
                )
                .child({
                    let label = div()
                        .id(("param-type", idx))
                        .w(px(s(70.0)))
                        .flex_shrink_0()
                        .text_size(px(Theme::font_size_ratio(0.78)))
                        .text_color(Theme::text_dim())
                        .child(row.spec.ty.label());
                    if row.spec.ty == ParamType::Auto {
                        label
                            .app_tooltip_text(crate::localization::text(
                                "query.params_strip.auto_type_tooltip",
                            ))
                            .into_any_element()
                    } else {
                        label.into_any_element()
                    }
                });
            if let Some(input) = &row.input {
                row_el = row_el.child(div().flex_1().child(Input::new(input)));
                let current = row.value.trim();
                let suggestions = &suggestion_sets[idx];
                let show_chips = suggestions.iter().any(|suggestion| {
                    current.is_empty() || !current.eq_ignore_ascii_case(suggestion)
                });
                if show_chips {
                    for (chip_i, suggestion) in
                        suggestions.iter().take(MAX_VALUES_PER_NAME).enumerate()
                    {
                        if !current.is_empty() && current.eq_ignore_ascii_case(suggestion) {
                            continue;
                        }
                        let full_value = suggestion.clone();
                        let label = if full_value.chars().count() > 24 {
                            format!("{}…", full_value.chars().take(23).collect::<String>())
                        } else {
                            full_value.clone()
                        };
                        let chip_input = input.clone();
                        row_el = row_el.child(
                            Button::new(("param-suggest", idx * 16 + chip_i))
                                .label(label)
                                .ghost()
                                .xsmall()
                                .app_tooltip_text(full_value.clone())
                                .on_click(move |_, window, cx| {
                                    chip_input.update(cx, |state, cx| {
                                        state.set_value(full_value.clone(), window, cx);
                                    });
                                }),
                        );
                    }
                }
                // A GUID parameter gets a one-click generator that fills a fresh
                // random UUID into its field (the input's Change subscription
                // picks it up as the parameter value).
                if row.spec.ty == ParamType::Uuid {
                    let gen_input = input.clone();
                    row_el = row_el.child(
                        Button::new(("param-uuid-gen", idx))
                            .label(crate::localization::text(
                                "query.params_strip.generate_uuid_button",
                            ))
                            .ghost()
                            .xsmall()
                            .app_tooltip_text(crate::localization::text(
                                "query.params_strip.generate_uuid_tooltip",
                            ))
                            .on_click(move |_, window, cx| {
                                let value = uuid::Uuid::new_v4().to_string();
                                gen_input
                                    .update(cx, |state, cx| state.set_value(value, window, cx));
                            }),
                    );
                }
            }
            if let Some((text, is_error)) = status {
                row_el = row_el.child(
                    div()
                        .flex_shrink_0()
                        .max_w(px(s(260.0)))
                        .truncate()
                        .text_size(px(Theme::font_size_ratio(0.78)))
                        .text_color(if is_error {
                            Theme::warning()
                        } else {
                            Theme::text_dim()
                        })
                        .child(text),
                );
            }
            rows_el = rows_el.child(row_el);
        }

        Some(
            div()
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(Theme::border())
                .bg(Theme::bg())
                .font(Theme::ui_font())
                .child(
                    div()
                        .px(px(s(12.0)))
                        .py(px(s(2.0)))
                        .text_size(px(Theme::font_size_ratio(0.75)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("query.params_strip.header")),
                )
                .child(
                    div()
                        .id("query-params-rows")
                        .max_h(px(s(110.0)))
                        .overflow_y_scroll()
                        .child(rows_el),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_params::ParamType;

    fn row(name: &str, ty: ParamType, value: &str) -> ParamRow {
        ParamRow {
            spec: ParamSpec {
                name: name.to_string(),
                ty,
                default: None,
            },
            input: None,
            value: value.to_string(),
            _subscription: None,
        }
    }

    #[test]
    fn saved_values_restore_onto_the_buffers_declarations() {
        let sql = "-- sqlly param: @from date\n-- sqlly param: @n int = 5\n\
                   -- sqlly param: @note text\nSELECT @from, @n, @note";
        let specs = crate::query_params::parse_param_directives(sql);
        let entered: Vec<(ParamSpec, String)> = specs
            .iter()
            .cloned()
            .zip(["2026-01-31", "42", ""])
            .map(|(spec, value)| (spec, value.to_string()))
            .collect();
        let saved = saved_param_values(entered);
        // Empty values are not written.
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[1].ty, "int");

        let restored = ParamStripState::from_persisted(&saved);
        let merged = merge_param_row_specs(&restored.rows, &specs);
        let values: Vec<&str> = merged.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(values, ["2026-01-31", "42", ""]);
        // The buffer's spec (with its default) wins over the saved one.
        assert_eq!(merged[1].0.default.as_deref(), Some("5"));
    }

    #[test]
    fn restored_values_drop_on_type_change_and_unknown_types() {
        let saved = vec![
            crate::workspace_session::SavedParamValue {
                name: "@id".into(),
                ty: "int".into(),
                value: "7".into(),
            },
            crate::workspace_session::SavedParamValue {
                name: "@x".into(),
                ty: "hologram".into(),
                value: "?".into(),
            },
        ];
        let restored = ParamStripState::from_persisted(&saved);
        assert_eq!(restored.rows.len(), 1);
        // `@id` is now declared as uuid: the int value must not carry over.
        let specs = crate::query_params::parse_param_directives("-- sqlly param: @id uuid");
        let merged = merge_param_row_specs(&restored.rows, &specs);
        assert_eq!(merged[0].1, "");
    }

    #[test]
    fn merge_reuses_value_when_name_and_type_match() {
        let old = vec![
            row("@a", ParamType::Int, "1"),
            row("@b", ParamType::Text, "hello"),
        ];
        let specs = vec![
            ParamSpec {
                name: "@A".into(),
                ty: ParamType::Int,
                default: None,
            },
            ParamSpec {
                name: "@c".into(),
                ty: ParamType::Int,
                default: None,
            },
        ];
        let merged = merge_param_row_specs(&old, &specs);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].0.name, "@A");
        assert_eq!(merged[0].1, "1");
        assert_eq!(merged[1].0.name, "@c");
        assert!(merged[1].1.is_empty());
    }

    #[test]
    fn merge_drops_value_when_type_changes() {
        let old = vec![row("@a", ParamType::Int, "42")];
        let specs = vec![ParamSpec {
            name: "@a".into(),
            ty: ParamType::Text,
            default: None,
        }];
        let merged = merge_param_row_specs(&old, &specs);
        assert_eq!(merged[0].1, "");
    }

    #[test]
    fn param_rows_match_specs_requires_exact_order_and_equality() {
        let rows = vec![row("@a", ParamType::Int, "")];
        let specs = vec![ParamSpec {
            name: "@a".into(),
            ty: ParamType::Int,
            default: None,
        }];
        assert!(param_rows_match_specs(&rows, &specs));
        let reordered = vec![ParamSpec {
            name: "@b".into(),
            ty: ParamType::Int,
            default: None,
        }];
        assert!(!param_rows_match_specs(&rows, &reordered));
    }

    fn spec(name: &str, ty: ParamType) -> ParamSpec {
        ParamSpec {
            name: name.to_string(),
            ty,
            default: None,
        }
    }

    /// Move Tab to Window: the seeded strip carries every value across, and
    /// the target's reconcile with the (unchanged) buffer keeps them, where
    /// before the move the target started with an empty strip.
    #[test]
    fn seeded_strip_keeps_moved_values_through_the_buffer_reconcile() {
        let moved = vec![
            (spec("@from", ParamType::Date), "2026-01-01".to_string()),
            (spec("@limit", ParamType::Int), "50".to_string()),
        ];
        let state = ParamStripState::seeded(moved.clone());
        assert_eq!(
            state.values(),
            vec![
                ("@from".to_string(), "2026-01-01".to_string()),
                ("@limit".to_string(), "50".to_string()),
            ]
        );
        assert!(state.rows.iter().all(|row| row.input.is_none()));
        // The same declarations in the moved buffer: rows "match", but the
        // sync must still build inputs, and it reuses these values.
        let specs: Vec<ParamSpec> = moved.iter().map(|(spec, _)| spec.clone()).collect();
        assert!(param_rows_match_specs(&state.rows, &specs));
        let merged = merge_param_row_specs(&state.rows, &specs);
        assert_eq!(merged[0].1, "2026-01-01");
        assert_eq!(merged[1].1, "50");
    }

    /// The transfer reads the input's live text, not a possibly stale
    /// canonical value (a keystroke can race the move like it races F5).
    #[gpui::test]
    fn snapshot_prefers_the_inputs_live_text(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        struct Host;
        impl gpui::Render for Host {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div()
            }
        }
        let (_host, cx) = cx.add_window_view(|_window, _cx| Host);
        let input = cx.update(|window, cx| {
            cx.new(|cx| InputState::new(window, cx).default_value("typed-just-now"))
        });
        let state = ParamStripState {
            rows: vec![
                ParamRow {
                    spec: spec("@name", ParamType::Text),
                    input: Some(input),
                    value: "stale".to_string(),
                    _subscription: None,
                },
                row("@seeded", ParamType::Int, "7"),
            ],
            pending_focus: None,
            spec_cache: None,
        };
        let snapshot = cx.update(|_window, cx| state.snapshot(cx));
        assert_eq!(snapshot[0].1, "typed-just-now");
        assert_eq!(snapshot[1].1, "7", "an input-less row keeps its value");
    }
}
