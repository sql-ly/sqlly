//! Results formatting panes: date/bool/uuid/integer/decimal, overrides, export.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;
use crate::preferences::{ColumnFormat, ColumnZoneChoice};

/// A picker color in the serde-friendly form the preferences store.
fn hsla_to_rgba(color: gpui::Hsla) -> sqlly_datatable::RgbaColor {
    let rgba: gpui::Rgba = color.into();
    sqlly_datatable::RgbaColor::new(rgba.r, rgba.g, rgba.b, rgba.a)
}

impl PreferencesWindow {
    // ============================================================
    // Results > Date
    // ============================================================

    pub(super) fn render_results_date(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rf = self.store.result_formatting();
        let fmt = &rf.defaults.date_format;
        let preview = fmt.format.clone();
        let mode = fmt.time_zone_mode.clone();
        let align = fmt.alignment.clone();
        let mut items = vec![
            self.heading(&crate::localization::text("settings.results.heading.date")),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.format_pattern",
            )),
            self.prefs_input("prefs_date_format"),
            self.hint(&crate::localization::text(
                "settings.results.hint.yyyy_mm_dd_hh_mm_ss_yyyy_mm",
            )),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.time_zone",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.time_zone"),
                self.enum_select(
                    "date_tz_mode",
                    mode.clone(),
                    vec![
                        DateTimeZoneMode::AlwaysUtc,
                        DateTimeZoneMode::NativeAsStored,
                        DateTimeZoneMode::AlwaysLocal,
                        DateTimeZoneMode::AlwaysInTimeZone,
                    ],
                    |m| timezone_mode_label(m).to_string(),
                    |this, m, cx| {
                        this.set_rf_field(cx, move |r| r.defaults.date_format.time_zone_mode = m)
                    },
                    window,
                    cx,
                ),
            ),
        ];
        if matches!(mode, DateTimeZoneMode::AlwaysInTimeZone) {
            let identifier = fmt.time_zone_identifier.clone();
            items.push(self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.time_zone_id"),
                self.prefs_input("prefs_date_tz"),
            ));
            let options: Vec<DropdownOption> = chrono_tz::TZ_VARIANTS
                .iter()
                .map(|tz| {
                    let name = tz.name().to_string();
                    let selected = name == identifier.trim();
                    let on: DropdownHandler = std::rc::Rc::new({
                        let name = name.clone();
                        move |this: &mut Self, window, cx| {
                            let value = name.clone();
                            this.set_input_value("prefs_date_tz", &value, window, cx);
                            this.set_rf_field(cx, move |r| {
                                r.defaults.date_format.time_zone_identifier = value;
                            });
                        }
                    });
                    (name, selected, on)
                })
                .collect();
            items.push(self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.pick_zone"),
                self.searchable_dropdown(
                    "date_tz_pick",
                    "",
                    options,
                    crate::localization::text(
                        "settings.results.searchable_dropdown.search_time_zones",
                    ),
                    window,
                    cx,
                ),
            ));
            match crate::result_cell_format::resolve_zone(
                DateTimeZoneMode::AlwaysInTimeZone,
                &identifier,
            ) {
                Ok(crate::result_cell_format::ResolvedZone::Named(tz)) => {
                    use chrono::{Offset as _, TimeZone as _};
                    let seconds = tz
                        .offset_from_utc_datetime(&chrono::Utc::now().naive_utc())
                        .fix()
                        .local_minus_utc();
                    let sign = if seconds < 0 { '-' } else { '+' };
                    let abs = seconds.unsigned_abs();
                    let offset = format!("{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60);
                    items.push(self.hint(&crate::localization::format(
                        "settings.results.hint.zone_offset_now",
                        &[("offset", offset)],
                    )));
                }
                Err(message) => items.push(
                    div()
                        .text_color(Theme::error())
                        .text_size(px(Theme::font_size_ratio(0.92)))
                        .child(crate::localization::format(
                            "settings.results.message.message_falling_back_utc",
                            &[("message", &message.message())],
                        ))
                        .into_any_element(),
                ),
                Ok(_) => {}
            }
        }
        if !matches!(mode, DateTimeZoneMode::NativeAsStored) {
            items.push(self.hint(&crate::localization::text(
                "settings.results.hint.utc_assumption",
            )));
        }
        items.push(self.section_header(&crate::localization::text(
            "settings.results.section_header.integer_timestamps",
        )));
        items.push(self.labeled_row(
            &crate::localization::text("settings.results.labeled_row.epoch_auto_detect"),
            self.enum_select(
                "epoch_auto_detect",
                rf.epoch_auto_detect,
                crate::preferences::EpochAutoDetect::ALL.to_vec(),
                |m| m.label(),
                |this, m, cx| this.set_rf_field(cx, move |r| r.epoch_auto_detect = m),
                window,
                cx,
            ),
        ));
        items.push(self.hint(&crate::localization::text(
            "settings.results.hint.epoch_auto_detect",
        )));
        items.push(self.section_header(&crate::localization::text(
            "settings.results.section_header.sample_preview",
        )));
        items.push(self.labeled_row(
            &crate::localization::text("settings.results.labeled_row.sample"),
            self.read_only_value(&preview),
        ));
        items.push(self.section_header(&crate::localization::text(
            "settings.results.section_header.alignment",
        )));
        items.push(self.labeled_row(
            &crate::localization::text("settings.results.labeled_row.alignment"),
            self.alignment_select(
                "date_align",
                align,
                |this, a, cx| this.set_rf_field(cx, move |r| r.defaults.date_format.alignment = a),
                window,
                cx,
            ),
        ));
        self.pane_container(items)
    }

    // ============================================================
    // Results > Boolean
    // ============================================================

    pub(super) fn render_results_bool(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rf = self.store.result_formatting();
        let fmt = &rf.defaults.bool_format;
        let style = fmt.style.clone();
        let align = fmt.alignment.clone();
        let preview_true = rf.format_bool(true);
        let preview_false = rf.format_bool(false);
        let mut items = vec![
            self.heading(&crate::localization::text(
                "settings.results.heading.boolean",
            )),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.style",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.style"),
                self.enum_select(
                    "bool_style",
                    style.clone(),
                    BoolFormatStyle::all().to_vec(),
                    |s| s.display_title().to_string(),
                    |this, s, cx| this.set_rf_field(cx, move |r| r.defaults.bool_format.style = s),
                    window,
                    cx,
                ),
            ),
        ];
        if matches!(style, BoolFormatStyle::Custom) {
            items.push(self.section_header(&crate::localization::text(
                "settings.results.section_header.custom_values",
            )));
            items.push(self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.true"),
                self.prefs_input("prefs_bool_true"),
            ));
            items.push(self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.false"),
                self.prefs_input("prefs_bool_false"),
            ));
        }
        items.push(self.section_header(&crate::localization::text(
            "settings.results.section_header.sample_preview_2",
        )));
        items.push(self.labeled_row(
            &crate::localization::text("settings.results.labeled_row.sample_2"),
            self.read_only_value(&format!("{} / {}", preview_true, preview_false)),
        ));
        items.push(self.section_header(&crate::localization::text(
            "settings.results.section_header.alignment_2",
        )));
        items.push(self.labeled_row(
            &crate::localization::text("settings.results.labeled_row.alignment_2"),
            self.alignment_select(
                "bool_align",
                align,
                |this, a, cx| this.set_rf_field(cx, move |r| r.defaults.bool_format.alignment = a),
                window,
                cx,
            ),
        ));
        self.pane_container(items)
    }

    // ============================================================
    // Results > Unique ID
    // ============================================================

    pub(super) fn render_results_general(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rf = self.store.result_formatting();
        let type_color_rows = if rf.color_cells_by_type {
            self.type_color_rows(window, cx)
        } else {
            Vec::new()
        };
        let mut items = vec![
            self.heading(&crate::localization::text(
                "settings.results.heading.general",
            )),
            self.checkbox(
                "grid_animations",
                &crate::localization::text("settings.results.checkbox.enable_grid_animations"),
                !rf.disable_animations,
                cx,
                Self::toggle_animations,
            ),
            self.hint(&crate::localization::text(
                "settings.results.hint.grid_animations",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.auto_size_columns_up"),
                self.stepper(
                    "auto_size_dec",
                    "auto_size_inc",
                    rf.auto_size_max_chars as i32,
                    cx,
                    |this, cx| {
                        this.set_rf_field(cx, |r| {
                            r.auto_size_max_chars = r.auto_size_max_chars.saturating_sub(1)
                        })
                    },
                    |this, cx| {
                        this.set_rf_field(cx, |r| {
                            r.auto_size_max_chars = (r.auto_size_max_chars + 1).min(1000)
                        })
                    },
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.results.hint.auto_size_columns",
            )),
            self.checkbox(
                "color_cells_by_type",
                &crate::localization::text("settings.results.checkbox.color_cells_by_data_type"),
                rf.color_cells_by_type,
                cx,
                Self::toggle_color_cells_by_type,
            ),
            self.hint(&crate::localization::text(
                "settings.results.hint.color_cells_by_type",
            )),
        ];
        items.extend(type_color_rows);
        items.extend([
            self.checkbox(
                "syntax_color_structured",
                &crate::localization::text("settings.results.checkbox.syntax_color_structured"),
                rf.syntax_color_structured,
                cx,
                |this, cx| {
                    this.set_rf_field(cx, |r| {
                        r.syntax_color_structured = !r.syntax_color_structured;
                    })
                },
            ),
            self.hint(&crate::localization::text(
                "settings.results.hint.syntax_color_structured",
            )),
            self.checkbox(
                "show_fk_labels",
                &crate::localization::text("settings.results.checkbox.show_foreign_key_labels"),
                rf.show_fk_labels,
                cx,
                Self::toggle_show_fk_labels,
            ),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.fk_label_mode"),
                self.enum_select(
                    "fk_label_mode",
                    rf.fk_label_mode,
                    crate::preferences::FkLabelMode::ALL.to_vec(),
                    |m| m.label().to_string(),
                    |this, mode, cx| this.set_rf_field(cx, move |r| r.fk_label_mode = mode),
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.row_density"),
                self.enum_select(
                    "grid_density",
                    rf.density,
                    GridDensity::ALL.to_vec(),
                    |d| d.label().to_string(),
                    |this, d, cx| this.set_rf_field(cx, move |r| r.density = d),
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.cell_details_dock"),
                self.enum_select(
                    "cell_detail_side",
                    rf.cell_detail_side,
                    CellDetailSide::ALL.to_vec(),
                    |s| s.label().to_string(),
                    |this, s, cx| this.set_rf_field(cx, move |r| r.cell_detail_side = s),
                    window,
                    cx,
                ),
            ),
            self.section_header(&crate::localization::text(
                "map_view.settings.section_header",
            )),
            self.checkbox(
                "map_tiles",
                &crate::localization::text("map_view.settings.load_tiles"),
                crate::preferences::MapPreferences::load().tiles
                    == crate::preferences::MapTileChoice::Allowed,
                cx,
                |_this, cx| {
                    use crate::preferences::{MapPreferences, MapTileChoice};
                    let mut prefs = MapPreferences::load();
                    prefs.tiles = if prefs.tiles == MapTileChoice::Allowed {
                        MapTileChoice::Declined
                    } else {
                        MapTileChoice::Allowed
                    };
                    prefs.save();
                    cx.notify();
                },
            ),
            self.hint(&crate::localization::text("map_view.settings.hint")),
        ]);
        self.pane_container(items)
    }

    // ============================================================
    // Results > History
    // ============================================================

    pub(super) fn render_results_history(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let perf = self.store.performance();
        let enabled = perf.result_run_history_enabled();
        let run_history = perf.result_run_history_runs() as i32;
        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.results.heading.history",
            )),
            self.checkbox(
                "run_history_enabled",
                &crate::localization::text("settings.results.checkbox.keep_history_recent_runs"),
                enabled,
                cx,
                Self::toggle_run_history_enabled,
            ),
            self.hint(&crate::localization::text(
                "settings.results.hint.run_history",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.runs_keep"),
                self.stepper(
                    "run_history_dec",
                    "run_history_inc",
                    run_history,
                    cx,
                    Self::dec_run_history,
                    Self::inc_run_history,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.results.hint.runs_to_keep",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.store_run"),
                self.enum_select(
                    "run_history_capture",
                    perf.result_run_history_capture(),
                    crate::preferences::RunHistoryCaptureMode::ALL.to_vec(),
                    |m| m.label().to_string(),
                    |this, mode, cx| {
                        this.set_performance_field(cx, move |p| p.result_run_history_capture = mode)
                    },
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.results.hint.store_run_condition",
            )),
        ])
    }

    pub(super) fn render_results_uuid(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rf = self.store.result_formatting();
        let preview = rf.format_uuid("550e8400-e29b-41d4-a716-446655440000");
        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.results.heading.unique_id",
            )),
            self.checkbox(
                "uuid_dashes",
                &crate::localization::text("settings.results.checkbox.show_dashes"),
                rf.defaults.uuid_format.show_dashes,
                cx,
                Self::toggle_uuid_dashes,
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.sample_preview_3",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.sample_3"),
                self.read_only_value(&preview),
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.alignment_3",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.alignment_3"),
                self.alignment_select(
                    "uuid_align",
                    rf.defaults.uuid_format.alignment.clone(),
                    |this, a, cx| {
                        this.set_rf_field(cx, move |r| r.defaults.uuid_format.alignment = a)
                    },
                    window,
                    cx,
                ),
            ),
        ])
    }

    // ============================================================
    // Results > Integer
    // ============================================================

    pub(super) fn render_results_integer(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rf = self.store.result_formatting();
        let fmt = &rf.defaults.integer_format;
        let preview = rf.format_integer(1234567);
        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.results.heading.integer",
            )),
            self.checkbox(
                "int_thousands",
                &crate::localization::text("settings.results.checkbox.use_thousands_separators"),
                fmt.use_thousands_separators,
                cx,
                Self::toggle_int_thousands,
            ),
            self.checkbox(
                "int_metric",
                &crate::localization::text("settings.results.checkbox.integer_metric_suffixes"),
                fmt.reduce_large_to_metric,
                cx,
                Self::toggle_int_metric,
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.metric",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.metric_threshold"),
                self.prefs_input("prefs_int_threshold"),
            ),
            self.hint(&crate::localization::format(
                "settings.results.message.labels_k_m_g_t_p",
                &[
                    ("kilo", &(fmt.metric_labels.kilo).to_string()),
                    ("mega", &(fmt.metric_labels.mega).to_string()),
                    ("giga", &(fmt.metric_labels.giga).to_string()),
                    ("tera", &(fmt.metric_labels.tera).to_string()),
                    ("peta", &(fmt.metric_labels.peta).to_string()),
                ],
            )),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.negative_format",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.negative"),
                self.enum_select(
                    "int_neg",
                    fmt.negative_format,
                    NegativeFormatStyle::all().to_vec(),
                    |n| n.display_title().to_string(),
                    |this, n, cx| {
                        this.set_rf_field(cx, move |r| {
                            r.defaults.integer_format.negative_format = n
                        })
                    },
                    window,
                    cx,
                ),
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.sample_preview_4",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.sample_4"),
                self.read_only_value(&preview),
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.alignment_4",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.alignment_4"),
                self.alignment_select(
                    "int_align",
                    fmt.alignment.clone(),
                    |this, a, cx| {
                        this.set_rf_field(cx, move |r| r.defaults.integer_format.alignment = a)
                    },
                    window,
                    cx,
                ),
            ),
        ])
    }

    // ============================================================
    // Results > Decimal
    // ============================================================

    pub(super) fn render_results_decimal(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rf = self.store.result_formatting();
        let fmt = &rf.defaults.decimal_format;
        let preview = rf.format_decimal(std::f64::consts::PI);
        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.results.heading.decimal",
            )),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.decimal_places",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.places"),
                self.stepper(
                    "dec_places_dec",
                    "dec_places_inc",
                    fmt.decimal_places,
                    cx,
                    Self::dec_places_dec,
                    Self::dec_places_inc,
                ),
            ),
            self.checkbox(
                "dec_thousands",
                &crate::localization::text("settings.results.checkbox.use_thousands_separators_2"),
                fmt.use_thousands_separators,
                cx,
                Self::toggle_dec_thousands,
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.decimal_separator",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.decimal_separator"),
                self.enum_select(
                    "dec_sep",
                    fmt.decimal_separator.clone(),
                    DecimalSeparator::all().to_vec(),
                    |s| s.display_title().to_string(),
                    |this, s, cx| {
                        this.set_rf_field(cx, move |r| {
                            r.defaults.decimal_format.decimal_separator = s
                        })
                    },
                    window,
                    cx,
                ),
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.thousands_separator",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.thousands_separator"),
                self.enum_select(
                    "dec_thou_sep",
                    fmt.thousands_separator.clone(),
                    ThousandsSeparator::all().to_vec(),
                    |s| s.display_title().to_string(),
                    |this, s, cx| {
                        this.set_rf_field(cx, move |r| {
                            r.defaults.decimal_format.thousands_separator = s
                        })
                    },
                    window,
                    cx,
                ),
            ),
            self.checkbox(
                "dec_metric",
                &crate::localization::text("settings.results.checkbox.decimal_metric_suffixes"),
                fmt.reduce_large_to_metric,
                cx,
                Self::toggle_dec_metric,
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.metric_2",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.metric_threshold_2"),
                self.prefs_input("prefs_dec_threshold"),
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.negative_format_2",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.negative_2"),
                self.enum_select(
                    "dec_neg",
                    fmt.negative_format,
                    NegativeFormatStyle::all().to_vec(),
                    |n| n.display_title().to_string(),
                    |this, n, cx| {
                        this.set_rf_field(cx, move |r| {
                            r.defaults.decimal_format.negative_format = n
                        })
                    },
                    window,
                    cx,
                ),
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.sample_preview_5",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.sample_5"),
                self.read_only_value(&preview),
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.alignment_5",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.alignment_5"),
                self.alignment_select(
                    "dec_align",
                    fmt.alignment.clone(),
                    |this, a, cx| {
                        this.set_rf_field(cx, move |r| r.defaults.decimal_format.alignment = a)
                    },
                    window,
                    cx,
                ),
            ),
        ])
    }

    // ============================================================
    // Results > Column overrides
    // ============================================================

    pub(super) fn render_results_overrides(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rf = self.store.result_formatting();
        let kind = self.override_kind;
        let mut items = vec![
            self.heading(&crate::localization::text(
                "settings.results.heading.column_overrides",
            )),
            self.intro(&crate::localization::text(
                "settings.results.intro.column_overrides",
            )),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.overrides",
            )),
            self.hint(&crate::localization::format(
                "settings.results.message.override_s_configured",
                &[("value", &(rf.column_overrides.len()).to_string())],
            )),
        ];
        // Overrides used to render as muted "• name" text with no way to delete
        // them: add-only, and the only exit was editing the JSON by hand.
        for (key, fmt) in rf.column_overrides.iter() {
            let key_owned = key.clone();
            items.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::scaled(8.)))
                    .py(px(Theme::scaled(2.)))
                    .child(
                        div()
                            .min_w(px(Theme::scaled(180.)))
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .child(self.match_text(key)),
                    )
                    .child(self.quiet_button(
                        SharedString::from(format!("override_remove_{key}")),
                        &crate::localization::text("settings.results.quiet_button.remove"),
                        cx,
                        move |this, _window, cx| {
                            let key = key_owned.clone();
                            this.set_rf_field(cx, |r| {
                                r.column_overrides.remove(&key);
                            });
                        },
                    ))
                    .into_any_element(),
            );
            match fmt.kind {
                ResultKind::Date => {
                    items.extend(self.column_zone_rows(
                        key,
                        &ColumnZoneChoice::DATE,
                        fmt.date_format.zone_choice(),
                        &fmt.date_format.time_zone_identifier,
                        |entry, choice| entry.date_format.set_zone_choice(choice),
                        |entry, id| entry.date_format.time_zone_identifier = id,
                        window,
                        cx,
                    ));
                }
                ResultKind::Mask => {
                    let prefix = fmt.mask_format.keep_prefix;
                    let suffix = fmt.mask_format.keep_suffix;
                    let prefix_id = intern_prefs_id(format!("override_mask_prefix_{key}"));
                    let suffix_id = intern_prefs_id(format!("override_mask_suffix_{key}"));
                    let key_prefix = key.clone();
                    let key_suffix = key.clone();
                    items.push(self.labeled_row(
                        &crate::localization::text("settings.results.labeled_row.visible_prefix"),
                        self.enum_select(
                            prefix_id,
                            prefix,
                            mask_keep_choices(prefix),
                            |n| n.to_string(),
                            move |this, n, cx| {
                                let key = key_prefix.clone();
                                this.set_rf_field(cx, move |r| {
                                    if let Some(entry) = r.column_overrides.get_mut(&key) {
                                        entry.mask_format.keep_prefix = n;
                                    }
                                });
                            },
                            window,
                            cx,
                        ),
                    ));
                    items.push(self.labeled_row(
                        &crate::localization::text("settings.results.labeled_row.visible_suffix"),
                        self.enum_select(
                            suffix_id,
                            suffix,
                            mask_keep_choices(suffix),
                            |n| n.to_string(),
                            move |this, n, cx| {
                                let key = key_suffix.clone();
                                this.set_rf_field(cx, move |r| {
                                    if let Some(entry) = r.column_overrides.get_mut(&key) {
                                        entry.mask_format.keep_suffix = n;
                                    }
                                });
                            },
                            window,
                            cx,
                        ),
                    ));
                }
                ResultKind::Epoch => {
                    let unit = fmt.epoch_format.unit;
                    let pattern = fmt.epoch_format.pattern.clone();
                    let unit_id = intern_prefs_id(format!("override_epoch_unit_{key}"));
                    let pattern_id = intern_prefs_id(format!("override_epoch_pattern_{key}"));
                    let key_unit = key.clone();
                    let key_pattern = key.clone();
                    items.push(self.labeled_row(
                        &crate::localization::text("settings.results.labeled_row.epoch_unit"),
                        self.enum_select(
                            unit_id,
                            unit,
                            EpochUnit::ALL.to_vec(),
                            |u| u.label().to_string(),
                            move |this, u, cx| {
                                let key = key_unit.clone();
                                this.set_rf_field(cx, move |r| {
                                    if let Some(entry) = r.column_overrides.get_mut(&key) {
                                        entry.epoch_format.unit = u;
                                    }
                                });
                            },
                            window,
                            cx,
                        ),
                    ));
                    items.push(self.labeled_row(
                        &crate::localization::text("settings.results.labeled_row.datetime_pattern"),
                        self.enum_select(
                            pattern_id,
                            pattern.clone(),
                            epoch_pattern_choices(&pattern),
                            |p| p.clone(),
                            move |this, p, cx| {
                                let key = key_pattern.clone();
                                this.set_rf_field(cx, move |r| {
                                    if let Some(entry) = r.column_overrides.get_mut(&key) {
                                        entry.epoch_format.pattern = p;
                                    }
                                });
                            },
                            window,
                            cx,
                        ),
                    ));
                    items.extend(self.column_zone_rows(
                        key,
                        &ColumnZoneChoice::EPOCH,
                        fmt.epoch_format.zone_choice(),
                        &fmt.epoch_format.time_zone_identifier,
                        |entry, choice| entry.epoch_format.set_zone_choice(choice),
                        |entry, id| entry.epoch_format.time_zone_identifier = id,
                        window,
                        cx,
                    ));
                }
                ResultKind::JsonPath => {
                    let path = fmt.json_path_format.path.clone();
                    let missing = fmt.json_path_format.missing_text.clone();
                    let path_id = intern_prefs_id(format!("override_json_path_{key}"));
                    let missing_id = intern_prefs_id(format!("override_json_missing_{key}"));
                    let key_path = key.clone();
                    let key_missing = key.clone();
                    items.push(self.labeled_row(
                        &crate::localization::text("settings.results.labeled_row.json_path"),
                        self.override_text_input(
                            path_id,
                            &path,
                            crate::localization::text(
                                "settings.results.override_text_input.customer_name",
                            ),
                            move |this, v, cx| {
                                let key = key_path.clone();
                                this.set_rf_field(cx, move |r| {
                                    if let Some(entry) = r.column_overrides.get_mut(&key) {
                                        entry.json_path_format.path = v;
                                    }
                                });
                            },
                            window,
                            cx,
                        ),
                    ));
                    items.push(self.labeled_row(
                        &crate::localization::text("settings.results.labeled_row.missing"),
                        self.override_text_input(
                            missing_id,
                            &missing,
                            crate::localization::text("settings.results.override_text_input.empty"),
                            move |this, v, cx| {
                                let key = key_missing.clone();
                                this.set_rf_field(cx, move |r| {
                                    if let Some(entry) = r.column_overrides.get_mut(&key) {
                                        entry.json_path_format.missing_text = v;
                                    }
                                });
                            },
                            window,
                            cx,
                        ),
                    ));
                    items.push(self.hint(&crate::localization::text(
                        "settings.results.hint.json_path_syntax",
                    )));
                }
                ResultKind::Template => {
                    let template = fmt.template_format.template.clone();
                    let template_id = intern_prefs_id(format!("override_template_{key}"));
                    let key_template = key.clone();
                    items.push(self.labeled_row(
                        &crate::localization::text("settings.results.labeled_row.template"),
                        self.override_text_input(
                            template_id,
                            &template,
                            crate::localization::text(
                                "settings.results.override_text_input.value_upper",
                            ),
                            move |this, v, cx| {
                                let key = key_template.clone();
                                this.set_rf_field(cx, move |r| {
                                    if let Some(entry) = r.column_overrides.get_mut(&key) {
                                        entry.template_format.template = v;
                                    }
                                });
                            },
                            window,
                            cx,
                        ),
                    ));
                    items.push(self.hint(&crate::localization::text(
                        "settings.results.hint.template_helpers",
                    )));
                }
                ResultKind::Number => {
                    let style = fmt.number_style_format.style;
                    let decimals = fmt.number_style_format.decimals;
                    let thousands = fmt.number_style_format.use_thousands_separators;
                    let style_id = intern_prefs_id(format!("override_num_style_{key}"));
                    let dec_id = intern_prefs_id(format!("override_num_dec_{key}"));
                    let inc_id = intern_prefs_id(format!("override_num_inc_{key}"));
                    let thousands_id = intern_prefs_id(format!("override_num_thousands_{key}"));
                    let key_style = key.clone();
                    let key_dec = key.clone();
                    let key_inc = key.clone();
                    let key_thousands = key.clone();
                    items.push(self.labeled_row(
                        &crate::localization::text("settings.results.labeled_row.style_2"),
                        self.enum_select(
                            style_id,
                            style,
                            NumberStyle::ALL.to_vec(),
                            |s| s.label().to_string(),
                            move |this, s, cx| {
                                let key = key_style.clone();
                                this.set_rf_field(cx, move |r| {
                                    if let Some(entry) = r.column_overrides.get_mut(&key) {
                                        entry.number_style_format.style = s;
                                    }
                                });
                            },
                            window,
                            cx,
                        ),
                    ));
                    items.push(self.labeled_row(
                        &crate::localization::text("settings.results.labeled_row.decimals"),
                        self.stepper(
                            dec_id,
                            inc_id,
                            i32::from(decimals),
                            cx,
                            move |this, cx| {
                                let key = key_dec.clone();
                                this.set_rf_field(cx, move |r| {
                                    if let Some(entry) = r.column_overrides.get_mut(&key) {
                                        entry.number_style_format.decimals =
                                            entry.number_style_format.decimals.saturating_sub(1);
                                    }
                                });
                            },
                            move |this, cx| {
                                let key = key_inc.clone();
                                this.set_rf_field(cx, move |r| {
                                    if let Some(entry) = r.column_overrides.get_mut(&key) {
                                        entry.number_style_format.decimals =
                                            (entry.number_style_format.decimals + 1).min(10);
                                    }
                                });
                            },
                        ),
                    ));
                    items.push(self.checkbox(
                        thousands_id,
                        &crate::localization::text(
                            "settings.results.checkbox.use_thousands_separators_3",
                        ),
                        thousands,
                        cx,
                        move |this, cx| {
                            let key = key_thousands.clone();
                            this.set_rf_field(cx, |r| {
                                if let Some(entry) = r.column_overrides.get_mut(&key) {
                                    entry.number_style_format.use_thousands_separators =
                                        !entry.number_style_format.use_thousands_separators;
                                }
                            });
                        },
                    ));
                }
                _ => {}
            }
        }
        items.push(self.section_header(&crate::localization::text(
            "settings.results.section_header.add_override",
        )));
        items.push(self.labeled_row(
            &crate::localization::text("settings.results.labeled_row.kind"),
            self.enum_select(
                "override_kind",
                kind,
                vec![
                    ResultKind::Date,
                    ResultKind::Bool,
                    ResultKind::Uuid,
                    ResultKind::Integer,
                    ResultKind::Decimal,
                    ResultKind::Mask,
                    ResultKind::Epoch,
                    ResultKind::JsonPath,
                    ResultKind::Template,
                    ResultKind::Number,
                ],
                |k| result_kind_label(k).to_string(),
                |this, k, cx| this.set_override_kind(k, cx),
                window,
                cx,
            ),
        ));
        items.push(self.labeled_row(
            &crate::localization::text("settings.results.labeled_row.column_identifier"),
            self.prefs_input("prefs_override_id"),
        ));
        if let Some(message) = self.field_errors.get("prefs_override_id") {
            items.push(self.warning(message));
        }
        items.push(
            div()
                .flex()
                .flex_row()
                .child(self.quiet_button(
                    "override_add",
                    &crate::localization::text("settings.results.quiet_button.add_override"),
                    cx,
                    |this, window, cx| {
                        this.add_column_override(window, cx);
                    },
                ))
                .into_any_element(),
        );
        items.push(self.checkbox(
            "export_masked",
            &crate::localization::text("settings.results.checkbox.mask_copies_and_exports"),
            rf.exports_masked,
            cx,
            |this, cx| {
                this.set_rf_field(cx, |r| r.exports_masked = !r.exports_masked);
            },
        ));
        self.pane_container(items)
    }

    // ============================================================
    // Results > Sensitive columns
    // ============================================================

    pub(super) fn render_results_masking(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::masking_rules::MaskingRules;
        use sqlly_core::RuleScope;

        let rules = self.store.masking_rules();
        let mut items = vec![
            self.heading(&crate::localization::text(
                "settings.results.heading.sensitive_columns",
            )),
            self.intro(&crate::localization::text(
                "settings.results.intro.sensitive_columns",
            )),
            self.checkbox(
                "masking_enabled",
                &crate::localization::text("settings.results.checkbox.auto_mask_sensitive_columns"),
                rules.enabled,
                cx,
                |this, cx| {
                    this.set_masking_field(cx, |m| m.enabled = !m.enabled);
                },
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.patterns",
            )),
        ];
        for (i, rule) in rules.patterns.iter().enumerate() {
            let scope = match rule.scope {
                RuleScope::ColumnName => {
                    crate::localization::text("settings.results.option.column_name")
                }
                RuleScope::TableDotColumn => "Table.column".to_string(),
            };
            items.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::scaled(8.)))
                    .py(px(Theme::scaled(2.)))
                    .child(
                        div()
                            .min_w(px(Theme::scaled(180.)))
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .child(self.match_text(&rule.pattern)),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(scope),
                    )
                    .child(self.quiet_button(
                        SharedString::from(format!("masking_remove_{i}")),
                        &crate::localization::text("settings.results.quiet_button.remove_2"),
                        cx,
                        move |this, _window, cx| {
                            this.set_masking_field(cx, |m| {
                                if i < m.patterns.len() {
                                    m.patterns.remove(i);
                                }
                            });
                        },
                    ))
                    .into_any_element(),
            );
        }
        items.push(self.input_row(
            &crate::localization::text("settings.results.input_row.add_pattern"),
            "masking_new_pattern",
        ));
        items.push(self.labeled_row(
            &crate::localization::text("settings.results.labeled_row.scope"),
            self.enum_select(
                "masking_new_scope",
                self.masking_new_scope,
                vec![RuleScope::ColumnName, RuleScope::TableDotColumn],
                |s| match s {
                    RuleScope::ColumnName => {
                        crate::localization::text("settings.results.option.column_name_2")
                    }
                    RuleScope::TableDotColumn => "Table.column".to_string(),
                },
                |this, s, cx| {
                    this.masking_new_scope = s;
                    cx.notify();
                },
                window,
                cx,
            ),
        ));
        items.push(
            div()
                .flex()
                .flex_row()
                .gap(px(Theme::scaled(8.)))
                .child(self.quiet_button(
                    "masking_add",
                    &crate::localization::text("settings.results.quiet_button.add"),
                    cx,
                    |this, window, cx| {
                        this.add_masking_pattern(window, cx);
                    },
                ))
                .child(self.quiet_button(
                    "masking_restore",
                    &crate::localization::text("settings.results.quiet_button.restore_defaults"),
                    cx,
                    |this, _window, cx| {
                        this.set_masking_field(cx, |m| *m = MaskingRules::default());
                    },
                ))
                .into_any_element(),
        );
        items.push(self.hint(&crate::localization::text(
            "settings.results.hint.email_not_masked",
        )));
        self.pane_container(items)
    }

    // ============================================================
    // Results > Export
    // ============================================================

    /// Checkbox element id for a standard export format toggle.
    pub(super) fn export_standard_checkbox_id(format: ExportFormat) -> &'static str {
        match format {
            ExportFormat::Csv => "export_csv",
            ExportFormat::Tsv => "export_tsv",
            ExportFormat::Json => "export_json",
            ExportFormat::JsonLines => "export_json_lines",
            ExportFormat::Markdown => "export_markdown",
            ExportFormat::Html => "export_html",
            ExportFormat::Xml => "export_xml",
            ExportFormat::Insert => "export_insert",
            ExportFormat::TempTable => "export_temp_table",
            ExportFormat::SqlValues => "export_sql_values",
            ExportFormat::Xlsx => "export_xlsx",
            _ => "export_unknown",
        }
    }

    /// Checkbox element id for a programming-language export toggle.
    pub(super) fn export_language_checkbox_id(language: CodeLanguage) -> &'static str {
        match language {
            CodeLanguage::CSharp => "export_lang_csharp",
            CodeLanguage::Python => "export_lang_python",
            CodeLanguage::Rust => "export_lang_rust",
            CodeLanguage::JavaScript => "export_lang_javascript",
            CodeLanguage::TypeScript => "export_lang_typescript",
            CodeLanguage::Kotlin => "export_lang_kotlin",
            CodeLanguage::Swift => "export_lang_swift",
            CodeLanguage::Java => "export_lang_java",
            CodeLanguage::Go => "export_lang_go",
            CodeLanguage::Php => "export_lang_php",
            CodeLanguage::Cpp => "export_lang_cpp",
        }
    }

    pub(super) fn render_results_export(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let export = self.store.export();
        let mut items = vec![
            self.heading(&crate::localization::text(
                "settings.results.heading.export",
            )),
            self.intro(&crate::localization::text(
                "settings.results.intro.export_formats",
            )),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.copy",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.default_copy_format"),
                self.enum_select(
                    "copy_format",
                    export.default_copy_format,
                    CopyFormat::ALL.to_vec(),
                    |f| f.label().to_string(),
                    |this, f, cx| this.set_export_field(cx, move |e| e.default_copy_format = f),
                    window,
                    cx,
                ),
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.spreadsheet_safety",
            )),
            self.checkbox(
                "export_formula_safe",
                &crate::localization::text("settings.results.checkbox.formula_protection"),
                export.formula_safe,
                cx,
                |this, cx| {
                    this.set_export_field(cx, |e| e.formula_safe = !e.formula_safe);
                },
            ),
            self.hint(&crate::localization::text(
                "settings.results.hint.formula_protection_scope",
            )),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.formats",
            )),
        ];
        for format in ExportFormat::all() {
            let format = *format;
            items.push(self.checkbox(
                Self::export_standard_checkbox_id(format),
                format.label(),
                export.standard_enabled(format),
                cx,
                move |this, cx| {
                    this.set_export_field(cx, move |e| e.toggle_standard(format));
                },
            ));
        }
        items.push(self.section_header(&crate::localization::text(
            "settings.results.section_header.programming",
        )));
        items.push(self.hint(&crate::localization::text(
            "settings.results.hint.programming_exports",
        )));
        items.push(self.checkbox(
            "export_programming_enabled",
            &crate::localization::text("settings.results.checkbox.show_code_exports"),
            export.programming_enabled,
            cx,
            |this, cx| {
                this.set_export_field(cx, |e| e.programming_enabled = !e.programming_enabled);
            },
        ));
        // Per-language toggles stay checked/unchecked independently of the
        // master switch, but are only interactive while the section is on.
        let languages_disabled = !export.programming_enabled;
        for language in CodeLanguage::all() {
            let language = *language;
            items.push(self.checkbox_disabled(
                Self::export_language_checkbox_id(language),
                language.label(),
                export.language_flag(language),
                languages_disabled,
                cx,
                move |this, cx| {
                    this.set_export_field(cx, move |e| e.toggle_language(language));
                },
            ));
        }
        self.pane_container(items)
    }

    pub(super) fn render_results_file_export(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::preferences::ExportOptionsPreferences;

        let opts = ExportOptionsPreferences::load();
        let null_mode = opts.null_mode.clone();
        let line_breaks = opts.line_breaks.clone();
        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.results.heading.file_export_options",
            )),
            self.intro(&crate::localization::text(
                "settings.results.intro.file_export_options",
            )),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.null_text",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.null_cells"),
                self.enum_select(
                    "export_null_mode",
                    null_mode,
                    vec![
                        "format-default".to_string(),
                        "empty".to_string(),
                        "literal".to_string(),
                        "custom".to_string(),
                    ],
                    |m| match m.as_str() {
                        "empty" => crate::localization::text("settings.results.option.empty"),
                        "literal" => "NULL".to_string(),
                        "custom" => crate::localization::text("settings.results.option.custom"),
                        _ => crate::localization::text("settings.results.option.format_default"),
                    },
                    |this, m, cx| {
                        this.set_export_options_field(cx, move |o| o.null_mode = m);
                    },
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.custom_null_text"),
                self.prefs_input("export_null_custom"),
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.line_breaks",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.embedded_line_breaks"),
                self.enum_select(
                    "export_line_breaks",
                    line_breaks,
                    vec![
                        "keep".to_string(),
                        "space".to_string(),
                        "escape".to_string(),
                    ],
                    |m| match m.as_str() {
                        "space" => {
                            crate::localization::text("settings.results.option.replace_space")
                        }
                        "escape" => crate::localization::text("settings.results.option.escape"),
                        _ => crate::localization::text("settings.results.option.keep"),
                    },
                    |this, m, cx| {
                        this.set_export_options_field(cx, move |o| o.line_breaks = m);
                    },
                    window,
                    cx,
                ),
            ),
            self.checkbox(
                "export_header_row",
                &crate::localization::text("settings.results.checkbox.include_header_row"),
                opts.header_row,
                cx,
                |this, cx| {
                    this.set_export_options_field(cx, |o| o.header_row = !o.header_row);
                },
            ),
            self.checkbox(
                "export_gzip",
                &crate::localization::text("settings.results.checkbox.gzip_compress_files"),
                opts.gzip,
                cx,
                |this, cx| {
                    this.set_export_options_field(cx, |o| o.gzip = !o.gzip);
                },
            ),
            self.section_header(&crate::localization::text(
                "settings.results.section_header.batch_export_pattern",
            )),
            self.labeled_row(
                &crate::localization::text("settings.results.labeled_row.filename_pattern"),
                self.prefs_input("export_filename_pattern"),
            ),
            self.hint(&crate::localization::text(
                "settings.results.hint.filename_pattern_tokens",
            )),
        ])
    }

    // ============================================================
    // Toggle handlers — Results formatting
    // ============================================================

    pub(super) fn toggle_animations(&mut self, cx: &mut Context<Self>) {
        self.set_rf_field(cx, |r| {
            r.disable_animations = !r.disable_animations;
        });
    }

    pub(super) fn toggle_show_fk_labels(&mut self, cx: &mut Context<Self>) {
        self.set_rf_field(cx, |r| {
            r.show_fk_labels = !r.show_fk_labels;
        });
    }

    pub(super) fn toggle_color_cells_by_type(&mut self, cx: &mut Context<Self>) {
        self.set_rf_field(cx, |r| {
            r.color_cells_by_type = !r.color_cells_by_type;
        });
    }

    pub(super) fn toggle_uuid_dashes(&mut self, cx: &mut Context<Self>) {
        self.set_rf_field(cx, |r| {
            r.defaults.uuid_format.show_dashes = !r.defaults.uuid_format.show_dashes;
        });
    }

    pub(super) fn toggle_int_thousands(&mut self, cx: &mut Context<Self>) {
        self.set_rf_field(cx, |r| {
            r.defaults.integer_format.use_thousands_separators =
                !r.defaults.integer_format.use_thousands_separators;
        });
    }

    pub(super) fn toggle_int_metric(&mut self, cx: &mut Context<Self>) {
        self.set_rf_field(cx, |r| {
            r.defaults.integer_format.reduce_large_to_metric =
                !r.defaults.integer_format.reduce_large_to_metric;
        });
    }

    pub(super) fn toggle_dec_thousands(&mut self, cx: &mut Context<Self>) {
        self.set_rf_field(cx, |r| {
            r.defaults.decimal_format.use_thousands_separators =
                !r.defaults.decimal_format.use_thousands_separators;
        });
    }

    pub(super) fn toggle_dec_metric(&mut self, cx: &mut Context<Self>) {
        self.set_rf_field(cx, |r| {
            r.defaults.decimal_format.reduce_large_to_metric =
                !r.defaults.decimal_format.reduce_large_to_metric;
        });
    }

    pub(super) fn dec_places_dec(&mut self, cx: &mut Context<Self>) {
        self.set_rf_field(cx, |r| {
            r.defaults.decimal_format.decimal_places =
                (r.defaults.decimal_format.decimal_places - 1).max(0);
        });
    }

    pub(super) fn dec_places_inc(&mut self, cx: &mut Context<Self>) {
        self.set_rf_field(cx, |r| {
            r.defaults.decimal_format.decimal_places =
                (r.defaults.decimal_format.decimal_places + 1).min(20);
        });
    }

    pub(super) fn dec_run_history(&mut self, cx: &mut Context<Self>) {
        self.set_performance_field(cx, |p| {
            p.result_run_history_runs = p.result_run_history_runs().saturating_sub(1);
        });
    }

    pub(super) fn inc_run_history(&mut self, cx: &mut Context<Self>) {
        self.set_performance_field(cx, |p| {
            p.result_run_history_runs = (p.result_run_history_runs() + 1).min(20);
        });
    }

    pub(super) fn toggle_run_history_enabled(&mut self, cx: &mut Context<Self>) {
        self.set_performance_field(cx, |p| {
            p.result_run_history_enabled = !p.result_run_history_enabled;
        });
    }

    // ---- Color pickers and per-column zones ----

    /// A color picker whose committed color routes through `apply` (`None`
    /// when cleared). Created on first render of `id`; later renders refresh
    /// the handler and re-sync the shown color from the store (e.g. after
    /// "Reset to theme").
    fn color_picker_field<A>(
        &self,
        id: &'static str,
        value: Option<gpui::Hsla>,
        apply: A,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        A: Fn(&mut Self, Option<gpui::Hsla>, &mut Context<Self>) + 'static,
    {
        use gpui_component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
        let mut slots = self.color_slots.borrow_mut();
        if let Some(slot) = slots.get_mut(id) {
            slot.apply.replace(std::rc::Rc::new(apply));
            if slot.state.read(cx).value() != value {
                slot.state.update(cx, |state, cx| match value {
                    Some(color) => state.set_value(color, window, cx),
                    None => state.clear_value(window, cx),
                });
            }
        } else {
            let apply_cell: std::rc::Rc<std::cell::RefCell<ColorApply>> =
                std::rc::Rc::new(std::cell::RefCell::new(std::rc::Rc::new(apply)));
            let state = cx.new(|cx| {
                let state = ColorPickerState::new(window, cx);
                match value {
                    Some(color) => state.default_value(color),
                    None => state,
                }
            });
            let subscription = cx.subscribe(&state, {
                let apply_cell = apply_cell.clone();
                move |this: &mut Self, _state, event: &ColorPickerEvent, cx| {
                    let ColorPickerEvent::Change(color) = event;
                    let apply = apply_cell.borrow().clone();
                    apply(this, *color, cx);
                }
            });
            slots.insert(
                id,
                ColorSlot {
                    state,
                    apply: apply_cell,
                    _subscription: subscription,
                },
            );
        }
        let state = slots.get(id).expect("slot inserted").state.clone();
        drop(slots);
        ColorPicker::new(&state).into_any_element()
    }

    /// One row per data kind under "Color cells by data type": a fill and a
    /// text color picker plus "Reset to theme".
    fn type_color_rows(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use crate::preferences::{TypeColorKind, TypeColorStyle};
        let rf = self.store.result_formatting();
        let mut rows = vec![self.hint(&crate::localization::text(
            "settings.results.hint.type_colors_custom",
        ))];
        for kind in TypeColorKind::ALL {
            let custom = rf.type_colors.get(&kind).copied().unwrap_or_default();
            let fill = custom
                .background
                .unwrap_or_else(|| crate::grid_bridge::theme_type_tint(kind));
            let set = move |this: &mut Self,
                            cx: &mut Context<Self>,
                            update: &dyn Fn(&mut TypeColorStyle)| {
                this.set_rf_field(cx, |r| {
                    let entry = r.type_colors.entry(kind).or_default();
                    update(entry);
                    if entry.is_empty() {
                        r.type_colors.remove(&kind);
                    }
                });
            };
            let fill_picker = self.color_picker_field(
                intern_prefs_id(format!("type_color_fill_{}", kind.key())),
                Some(fill.to_hsla()),
                move |this, color, cx| {
                    let rgba = color.map(hsla_to_rgba);
                    set(this, cx, &|entry| entry.background = rgba);
                },
                window,
                cx,
            );
            let text_picker = self.color_picker_field(
                intern_prefs_id(format!("type_color_text_{}", kind.key())),
                custom.foreground.map(|c| c.to_hsla()),
                move |this, color, cx| {
                    let rgba = color.map(hsla_to_rgba);
                    set(this, cx, &|entry| entry.foreground = rgba);
                },
                window,
                cx,
            );
            let reset = self.quiet_button(
                SharedString::from(format!("type_color_reset_{}", kind.key())),
                &crate::localization::text("settings.results.quiet_button.reset_to_theme"),
                cx,
                move |this, _window, cx| {
                    this.set_rf_field(cx, |r| {
                        r.type_colors.remove(&kind);
                    });
                },
            );
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::scaled(8.)))
                    .py(px(Theme::scaled(2.)))
                    .child(
                        div()
                            .min_w(px(Theme::scaled(140.)))
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .child(self.match_text(&kind.label())),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .text_color(Theme::text_dim())
                            .child(self.match_text(&crate::localization::text(
                                "settings.results.labeled_row.type_color_fill",
                            ))),
                    )
                    .child(fill_picker)
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .text_color(Theme::text_dim())
                            .child(self.match_text(&crate::localization::text(
                                "settings.results.labeled_row.type_color_text",
                            ))),
                    )
                    .child(text_picker)
                    .child(reset)
                    .into_any_element(),
            );
        }
        rows
    }

    /// The zone controls for one Date or Epoch column override: the choice
    /// (follow global / UTC / local / as stored / named) and, for a named
    /// zone, the identifier field plus the searchable zone list.
    #[allow(clippy::too_many_arguments)]
    fn column_zone_rows(
        &self,
        key: &str,
        choices: &[ColumnZoneChoice],
        current: ColumnZoneChoice,
        identifier: &str,
        set_choice: fn(&mut ColumnFormat, ColumnZoneChoice),
        set_identifier: fn(&mut ColumnFormat, String),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        let key_choice = key.to_string();
        rows.push(self.labeled_row(
            &crate::localization::text("settings.results.labeled_row.time_zone"),
            self.enum_select(
                intern_prefs_id(format!("override_zone_{key}")),
                current,
                choices.to_vec(),
                |c| c.label(),
                move |this, choice, cx| {
                    let key = key_choice.clone();
                    this.set_rf_field(cx, move |r| {
                        if let Some(entry) = r.column_overrides.get_mut(&key) {
                            set_choice(entry, choice);
                        }
                    });
                },
                window,
                cx,
            ),
        ));
        if current != ColumnZoneChoice::Named {
            return rows;
        }
        let key_text = key.to_string();
        rows.push(self.labeled_row(
            &crate::localization::text("settings.results.labeled_row.time_zone_id"),
            self.override_text_input(
                intern_prefs_id(format!("override_zone_id_{key}")),
                identifier,
                "Europe/Berlin".to_string(),
                move |this, value, cx| {
                    let key = key_text.clone();
                    this.set_rf_field(cx, move |r| {
                        if let Some(entry) = r.column_overrides.get_mut(&key) {
                            set_identifier(entry, value);
                        }
                    });
                },
                window,
                cx,
            ),
        ));
        let options: Vec<DropdownOption> = chrono_tz::TZ_VARIANTS
            .iter()
            .map(|tz| {
                let name = tz.name().to_string();
                let selected = name == identifier.trim();
                let key = key.to_string();
                let on: DropdownHandler = std::rc::Rc::new({
                    let name = name.clone();
                    move |this: &mut Self, _window, cx| {
                        let value = name.clone();
                        let key = key.clone();
                        this.set_rf_field(cx, move |r| {
                            if let Some(entry) = r.column_overrides.get_mut(&key) {
                                set_identifier(entry, value);
                            }
                        });
                    }
                });
                (name, selected, on)
            })
            .collect();
        rows.push(self.labeled_row(
            &crate::localization::text("settings.results.labeled_row.pick_zone"),
            self.searchable_dropdown(
                intern_prefs_id(format!("override_zone_pick_{key}")),
                "",
                options,
                crate::localization::text("settings.results.searchable_dropdown.search_time_zones"),
                window,
                cx,
            ),
        ));
        if let Err(message) =
            crate::result_cell_format::resolve_zone(DateTimeZoneMode::AlwaysInTimeZone, identifier)
        {
            rows.push(
                div()
                    .text_color(Theme::error())
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .child(crate::localization::format(
                        "settings.results.message.column_zone_falling_back_utc",
                        &[("message", &message.message())],
                    ))
                    .into_any_element(),
            );
        }
        rows
    }

    // ---- Column-overrides pane actions ----

    pub(super) fn set_override_kind(&mut self, kind: ResultKind, cx: &mut Context<Self>) {
        self.override_kind = kind;
        cx.notify();
    }

    pub(super) fn add_column_override(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The identifier lives in its input widget; there is no mirror string.
        let id = self
            .inputs
            .get("prefs_override_id")
            .map(|state| state.value(cx).trim().to_string())
            .unwrap_or_default();
        // Used to `return` here, so clicking Add with an empty field did
        // nothing at all — no override, no message, no clue which of the two
        // inputs was at fault.
        if id.is_empty() {
            self.field_errors.insert(
                "prefs_override_id",
                "Enter a column identifier, like sales.total or total.".to_string(),
            );
            cx.notify();
            return;
        }
        if self
            .store
            .result_formatting()
            .column_overrides
            .contains_key(&id)
        {
            self.field_errors.insert(
                "prefs_override_id",
                crate::localization::format(
                    "settings.results.message.override_id_already_exists",
                    &[("id", &(id).to_string())],
                ),
            );
            cx.notify();
            return;
        }
        self.field_errors.remove("prefs_override_id");
        let kind = self.override_kind;
        self.set_rf_field(cx, move |r| {
            r.column_overrides
                .entry(id)
                .or_insert_with(|| preferences::ColumnFormat::default_for_kind(kind));
        });
        self.set_input_value("prefs_override_id", "", window, cx);
    }

    pub(super) fn add_masking_pattern(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pattern = self
            .inputs
            .get("masking_new_pattern")
            .map(|state| state.value(cx).trim().to_string())
            .unwrap_or_default();
        if pattern.is_empty() {
            self.field_errors.insert(
                "masking_new_pattern",
                "Enter a glob like *phone* or a regex like /ssn|nin/.".to_string(),
            );
            cx.notify();
            return;
        }
        self.field_errors.remove("masking_new_pattern");
        let scope = self.masking_new_scope;
        self.set_masking_field(cx, move |m| {
            if !m
                .patterns
                .iter()
                .any(|rule| rule.pattern == pattern && rule.scope == scope)
            {
                m.patterns.push(sqlly_core::MaskRule {
                    pattern: pattern.clone(),
                    scope,
                });
            }
        });
        self.set_input_value("masking_new_pattern", "", window, cx);
    }
}

fn intern_prefs_id(s: String) -> &'static str {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static MAP: std::sync::OnceLock<Mutex<HashMap<String, &'static str>>> =
        std::sync::OnceLock::new();
    let mut map = MAP
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(&existing) = map.get(&s) {
        return existing;
    }
    let leaked: &'static str = Box::leak(s.clone().into_boxed_str());
    map.insert(s, leaked);
    leaked
}

fn mask_keep_choices(current: usize) -> Vec<usize> {
    let mut choices = vec![0, 1, 2, 4, 6, 8];
    if !choices.contains(&current) {
        choices.push(current);
        choices.sort_unstable();
    }
    choices
}

fn epoch_pattern_choices(current: &str) -> Vec<String> {
    let mut choices = vec![
        "%Y-%m-%d %H:%M:%S".to_string(),
        "%Y-%m-%d".to_string(),
        "%Y-%m-%d %H:%M:%S %:z".to_string(),
        "%d/%m/%Y %H:%M:%S".to_string(),
    ];
    if !choices.iter().any(|p| p == current) {
        choices.push(current.to_string());
    }
    choices
}
