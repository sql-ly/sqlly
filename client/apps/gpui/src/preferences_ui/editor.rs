//! Editor panes: general, formatting, IntelliSense, external editors.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

use crate::preferences::WeekStart;
use sqlly_intel::natural_date::{BareHour, DatePreference, RangeStyle, ToDateEnd};

fn month_name(month: u32) -> String {
    let keys = [
        "settings.editor.natural_dates.month.january",
        "settings.editor.natural_dates.month.february",
        "settings.editor.natural_dates.month.march",
        "settings.editor.natural_dates.month.april",
        "settings.editor.natural_dates.month.may",
        "settings.editor.natural_dates.month.june",
        "settings.editor.natural_dates.month.july",
        "settings.editor.natural_dates.month.august",
        "settings.editor.natural_dates.month.september",
        "settings.editor.natural_dates.month.october",
        "settings.editor.natural_dates.month.november",
        "settings.editor.natural_dates.month.december",
    ];
    crate::localization::text(keys[(month as usize).saturating_sub(1) % 12])
}

impl PreferencesWindow {
    // ============================================================
    // Editor > General
    // ============================================================

    pub(super) fn render_editor_general(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let e = self.store.editor();
        let is_tabs = matches!(e.indent_style, IndentStyle::Tabs);
        let is_spaces = matches!(e.indent_style, IndentStyle::Spaces);

        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.editor.heading.general",
            )),
            self.intro(&crate::localization::text(
                "settings.editor.intro.general_intro",
            )),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.display",
            )),
            self.checkbox(
                "show_ln",
                &crate::localization::text("settings.editor.checkbox.show_line_numbers"),
                e.show_line_numbers,
                cx,
                Self::toggle_show_line_numbers,
            ),
            self.checkbox(
                "show_stmt_frame",
                &crate::localization::text("settings.editor.checkbox.frame_current_statement"),
                e.show_statement_frame,
                cx,
                Self::toggle_show_statement_frame,
            ),
            self.checkbox(
                "show_hover_info",
                &crate::localization::text("settings.editor.checkbox.hover_info"),
                e.show_hover_info,
                cx,
                Self::toggle_show_hover_info,
            ),
            self.checkbox(
                "show_signature_help",
                &crate::localization::text("settings.editor.checkbox.signature_help"),
                e.show_signature_help,
                cx,
                Self::toggle_show_signature_help,
            ),
            self.checkbox(
                "show_insert_hints",
                &crate::localization::text("settings.editor.checkbox.insert_column_hints"),
                e.show_insert_value_hints,
                cx,
                Self::toggle_show_insert_value_hints,
            ),
            self.checkbox(
                "show_ai_summary_bar",
                &crate::localization::text("settings.editor.checkbox.show_ai_query_summary_bar"),
                self.store.ai_summary().enabled,
                cx,
                |this, cx| {
                    this.set_ai_summary_field(cx, |p| p.enabled = !p.enabled);
                },
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.ai_summary_bar",
            )),
            self.checkbox(
                "vim_mode",
                &crate::localization::text(
                    "settings.editor.checkbox.vim_keybindings_modal_editing",
                ),
                e.vim_mode,
                cx,
                Self::toggle_vim_mode,
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.vim_mode_keys",
            )),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.bold_text",
            )),
            self.hint(&crate::localization::text(
                "settings.editor.hint.bold_text_help",
            )),
            self.checkbox(
                "bold_keywords",
                &crate::localization::text("settings.editor.checkbox.keywords"),
                e.syntax_bold.keywords,
                cx,
                Self::toggle_bold_keywords,
            ),
            self.checkbox(
                "bold_literals",
                &crate::localization::text(
                    "settings.editor.checkbox.literals_strings_numbers_null",
                ),
                e.syntax_bold.literals,
                cx,
                Self::toggle_bold_literals,
            ),
            self.checkbox(
                "bold_functions",
                &crate::localization::text("settings.editor.checkbox.built_function_names"),
                e.syntax_bold.functions,
                cx,
                Self::toggle_bold_functions,
            ),
            self.checkbox(
                "bold_system_types",
                &crate::localization::text("settings.editor.checkbox.system_data_types"),
                e.syntax_bold.system_types,
                cx,
                Self::toggle_bold_system_types,
            ),
            self.checkbox(
                "bold_procedures",
                &crate::localization::text("settings.editor.checkbox.procedures"),
                e.syntax_bold.procedures,
                cx,
                Self::toggle_bold_procedures,
            ),
            self.checkbox(
                "bold_schemas",
                &crate::localization::text("settings.editor.checkbox.schemas"),
                e.syntax_bold.schemas,
                cx,
                Self::toggle_bold_schemas,
            ),
            self.checkbox(
                "bold_user_functions",
                &crate::localization::text("settings.editor.checkbox.user_defined_functions"),
                e.syntax_bold.user_functions,
                cx,
                Self::toggle_bold_user_functions,
            ),
            self.checkbox(
                "bold_tables",
                &crate::localization::text("settings.editor.checkbox.tables_views"),
                e.syntax_bold.tables,
                cx,
                Self::toggle_bold_tables,
            ),
            self.checkbox(
                "bold_user_types",
                &crate::localization::text("settings.editor.checkbox.user_defined_types"),
                e.syntax_bold.user_types,
                cx,
                Self::toggle_bold_user_types,
            ),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.navigation",
            )),
            self.checkbox(
                "camel_hump_words",
                &crate::localization::text("settings.editor.checkbox.camel_hump_word_movement"),
                e.camel_hump_word_movement,
                cx,
                Self::toggle_camel_hump_word_movement,
            ),
            self.hint(&if cfg!(target_os = "macos") {
                crate::localization::text("settings.editor.static.camel_hump_hint_option")
            } else {
                crate::localization::text("settings.editor.static.camel_hump_hint_ctrl")
            }),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.selection",
            )),
            self.checkbox(
                "highlight_occurrences",
                &crate::localization::text(
                    "settings.editor.checkbox.highlight_selection_occurrences",
                ),
                e.highlight_occurrences,
                cx,
                Self::toggle_highlight_occurrences,
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.highlight_occurrences_help",
            )),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.query_parameters",
            )),
            self.checkbox(
                "detect_query_placeholders",
                &crate::localization::text("settings.editor.checkbox.detect_query_placeholders"),
                e.detect_query_placeholders,
                cx,
                Self::toggle_detect_query_placeholders,
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.detect_query_placeholders_help",
            )),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.indentation",
            )),
            self.radio(
                "indent_tabs",
                &crate::localization::text("settings.editor.radio.tabs"),
                is_tabs,
                cx,
                Self::choose_tabs_indent,
            ),
            self.radio(
                "indent_spaces",
                &crate::localization::text("settings.editor.radio.spaces"),
                is_spaces,
                cx,
                Self::choose_spaces_indent,
            ),
            self.labeled_row(
                &crate::localization::text("settings.editor.labeled_row.spaces_per_tab"),
                self.stepper(
                    "sp_dec",
                    "sp_inc",
                    e.spaces_per_tab_raw,
                    cx,
                    Self::dec_spaces,
                    Self::inc_spaces,
                ),
            ),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.spacing",
            )),
            self.checkbox(
                "auto_sp",
                &crate::localization::text("settings.editor.checkbox.auto_spacing"),
                e.auto_spacing,
                cx,
                Self::toggle_auto_spacing,
            ),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.automagic_commas",
            )),
            self.hint(&crate::localization::text(
                "settings.editor.hint.automagic_commas_help",
            )),
            self.checkbox(
                "automagic_select",
                &crate::localization::text("settings.editor.checkbox.select_lists"),
                e.automagic_select_commas,
                cx,
                Self::toggle_automagic_select_commas,
            ),
            self.checkbox(
                "automagic_group_by",
                &crate::localization::text("settings.editor.checkbox.group_by_lists"),
                e.automagic_group_by_commas,
                cx,
                Self::toggle_automagic_group_by_commas,
            ),
            self.checkbox(
                "automagic_order_by",
                &crate::localization::text("settings.editor.checkbox.order_by_lists"),
                e.automagic_order_by_commas,
                cx,
                Self::toggle_automagic_order_by_commas,
            ),
            self.checkbox(
                "automagic_connectors",
                &crate::localization::text("settings.editor.checkbox.auto_insert_connectors"),
                e.automagic_where_connectors,
                cx,
                Self::toggle_automagic_where_connectors,
            ),
            self.labeled_row(
                &crate::localization::text("settings.editor.labeled_row.comma_placement"),
                div()
                    .flex()
                    .flex_row()
                    .gap(px(Theme::scaled(8.)))
                    .child(self.radio(
                        "automagic_trailing",
                        &crate::localization::text("settings.editor.radio.trailing"),
                        e.automagic_comma_placement == AutomagicCommaPlacement::Trailing,
                        cx,
                        Self::choose_automagic_trailing,
                    ))
                    .child(self.radio(
                        "automagic_leading",
                        &crate::localization::text("settings.editor.radio.leading"),
                        e.automagic_comma_placement == AutomagicCommaPlacement::Leading,
                        cx,
                        Self::choose_automagic_leading,
                    ))
                    .into_any_element(),
            ),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.intellisense",
            )),
            self.labeled_row(
                &crate::localization::text("settings.editor.labeled_row.source"),
                self.enum_select(
                    "intellisense_src",
                    e.intellisense_source.clone(),
                    vec![IntelliSenseSource::BuiltIn, IntelliSenseSource::RustEngine],
                    |s| intellisense_label(s).to_string(),
                    |this, s, cx| this.set_editor_field(cx, move |e| e.intellisense_source = s),
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.source_help",
            )),
            self.checkbox(
                "fb_full_model",
                &crate::localization::text("settings.editor.checkbox.feedback_full_model_default"),
                e.feedback_include_full_data_model,
                cx,
                Self::toggle_feedback_full_data_model,
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.feedback_full_model_help",
            )),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.feedback_report_object_kinds",
            )),
            self.intro(&crate::localization::text(
                "settings.editor.intro.feedback_kinds_intro",
            )),
            self.checkbox(
                "fb_kind_tables",
                &crate::localization::text("settings.editor.checkbox.tables"),
                e.feedback_show_tables,
                cx,
                Self::toggle_feedback_show_tables,
            ),
            self.checkbox(
                "fb_kind_views",
                &crate::localization::text("settings.editor.checkbox.views"),
                e.feedback_show_views,
                cx,
                Self::toggle_feedback_show_views,
            ),
            self.checkbox(
                "fb_kind_procedures",
                &crate::localization::text("settings.editor.checkbox.procedures_2"),
                e.feedback_show_procedures,
                cx,
                Self::toggle_feedback_show_procedures,
            ),
            self.checkbox(
                "fb_kind_functions",
                &crate::localization::text("settings.editor.checkbox.functions"),
                e.feedback_show_functions,
                cx,
                Self::toggle_feedback_show_functions,
            ),
            self.checkbox(
                "fb_kind_columns",
                &crate::localization::text("settings.editor.checkbox.individual_columns"),
                e.feedback_show_columns,
                cx,
                Self::toggle_feedback_show_columns,
            ),
            self.checkbox(
                "fb_kind_types",
                &crate::localization::text("settings.editor.checkbox.types"),
                e.feedback_show_types,
                cx,
                Self::toggle_feedback_show_types,
            ),
        ])
    }

    // ============================================================
    // Editor > IntelliSense
    // ============================================================

    pub(super) fn render_editor_intellisense(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let e = self.store.editor();
        let d = e.natural_dates;
        let is_caps = matches!(e.alias_generation, AliasGeneration::CapitalLettersLowercase);
        let is_auto = matches!(e.alias_generation, AliasGeneration::AutoBasedOnJoin);
        let is_dns = matches!(e.alias_generation, AliasGeneration::DoNotSuggest);

        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.editor.heading.intellisense",
            )),
            self.intro(&crate::localization::text(
                "settings.editor.intro.intellisense_intro",
            )),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.matching",
            )),
            self.checkbox(
                "is_hump_matching",
                &crate::localization::text("settings.editor.checkbox.match_camel_humps_initials"),
                e.hump_matching,
                cx,
                Self::toggle_hump_matching,
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.camel_hump_match_help",
            )),
            self.checkbox(
                "is_schema_search",
                &crate::localization::text(
                    "settings.editor.checkbox.include_schema_name_searching",
                ),
                e.schema_in_search,
                cx,
                Self::toggle_schema_in_search,
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.schema_name_search",
            )),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.alias_names",
            )),
            self.radio(
                "alias_caps",
                &crate::localization::text("settings.editor.radio.alias_from_capitals"),
                is_caps,
                cx,
                Self::choose_alias_caps,
            ),
            // Indented sub-option for the capitals strategy.
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(6.)))
                .pl(px(Theme::scaled(24.)))
                .child(self.checkbox(
                    "alias_include_schema",
                    &crate::localization::text("settings.editor.checkbox.alias_include_schema"),
                    e.include_schema_in_alias,
                    cx,
                    Self::toggle_include_schema,
                ))
                .into_any_element(),
            self.radio(
                "alias_auto",
                &crate::localization::text("settings.editor.radio.auto_based_join"),
                is_auto,
                cx,
                Self::choose_alias_auto,
            ),
            self.radio(
                "alias_dns",
                &crate::localization::text("settings.editor.radio.do_not_suggest"),
                is_dns,
                cx,
                Self::choose_alias_dns,
            ),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.accept_table_view",
            )),
            self.radio(
                "alias_add_auto",
                &crate::localization::text("settings.editor.radio.auto_add_alias"),
                e.auto_add_table_alias,
                cx,
                Self::choose_alias_add_automatically,
            ),
            self.radio(
                "alias_add_second",
                &crate::localization::text("settings.editor.radio.alias_on_second_accept"),
                !e.auto_add_table_alias,
                cx,
                Self::choose_alias_add_on_second_accept,
            ),
            self.checkbox(
                "alias_as_keyword",
                &crate::localization::text("settings.editor.checkbox.alias_as_keyword"),
                e.alias_as_keyword,
                cx,
                Self::toggle_alias_as_keyword,
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.alias_insert_scope",
            )),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.date_time_convenience",
            )),
            self.checkbox(
                "nd_enabled",
                &crate::localization::text("settings.editor.checkbox.read_dates_typed_plain_words"),
                d.enabled,
                cx,
                |this, cx| this.set_natural_dates(cx, |d| d.enabled = !d.enabled),
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.plain_dates_help",
            )),
            self.checkbox(
                "nd_ranges",
                &crate::localization::text("settings.editor.checkbox.turn_periods_into_ranges"),
                d.convert_ranges,
                cx,
                |this, cx| this.set_natural_dates(cx, |d| d.convert_ranges = !d.convert_ranges),
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.period_ranges_help",
            )),
            self.labeled_row(
                &crate::localization::text("settings.editor.labeled_row.range_style"),
                self.enum_select(
                    "nd_range_style",
                    d.range_style,
                    vec![RangeStyle::Between, RangeStyle::HalfOpen],
                    |s| match s {
                        RangeStyle::Between => "BETWEEN start AND end".to_string(),
                        RangeStyle::HalfOpen => "(>= start AND < next)".to_string(),
                    },
                    |this, v, cx| this.set_natural_dates(cx, |d| d.range_style = v),
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.range_style_help",
            )),
            self.labeled_row(
                &crate::localization::text("settings.editor.labeled_row.day_month_its_own"),
                self.enum_select(
                    "nd_prefer",
                    d.prefer,
                    vec![
                        DatePreference::Past,
                        DatePreference::Future,
                        DatePreference::Current,
                    ],
                    |p| match p {
                        DatePreference::Past => crate::localization::text(
                            "settings.editor.option.means_most_recent_one",
                        ),
                        DatePreference::Future => {
                            crate::localization::text("settings.editor.option.means_next_one")
                        }
                        DatePreference::Current => {
                            crate::localization::text("settings.editor.option.means_week_s_year_s")
                        }
                    },
                    |this, v, cx| this.set_natural_dates(cx, |d| d.prefer = v),
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.bare_day_month_help",
            )),
            self.labeled_row(
                &crate::localization::text("settings.editor.labeled_row.weeks_start"),
                self.enum_select(
                    "nd_week_start",
                    d.week_start,
                    vec![WeekStart::Sunday, WeekStart::Monday],
                    |w| match w {
                        WeekStart::Sunday => {
                            crate::localization::text("settings.editor.option.sunday")
                        }
                        WeekStart::Monday => {
                            crate::localization::text("settings.editor.option.monday")
                        }
                    },
                    |this, v, cx| this.set_natural_dates(cx, |d| d.week_start = v),
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text("settings.editor.labeled_row.time_without_am_pm"),
                self.enum_select(
                    "nd_bare_hour",
                    d.bare_hour,
                    vec![BareHour::BusinessHours, BareHour::TwentyFourHour],
                    |b| match b {
                        BareHour::BusinessHours => {
                            crate::localization::text("settings.editor.option.bare_hour_business")
                        }
                        BareHour::TwentyFourHour => {
                            crate::localization::text("settings.editor.option.bare_hour_24h")
                        }
                    },
                    |this, v, cx| this.set_natural_dates(cx, |d| d.bare_hour = v),
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text("settings.editor.labeled_row.date_periods_end"),
                self.enum_select(
                    "nd_to_date_end",
                    d.to_date_end,
                    vec![ToDateEnd::EndOfToday, ToDateEnd::Now],
                    |t| match t {
                        ToDateEnd::EndOfToday => {
                            crate::localization::text("settings.editor.option.at_end_today")
                        }
                        ToDateEnd::Now => {
                            crate::localization::text("settings.editor.option.right_now")
                        }
                    },
                    |this, v, cx| this.set_natural_dates(cx, |d| d.to_date_end = v),
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.to_date_range_end",
            )),
            self.labeled_row(
                &crate::localization::text("settings.editor.labeled_row.fiscal_year_starts"),
                self.enum_select(
                    "nd_fiscal_start",
                    d.fiscal_year_start_month.clamp(1, 12),
                    (1..=12).collect(),
                    |m| month_name(*m),
                    |this, v, cx| this.set_natural_dates(cx, |d| d.fiscal_year_start_month = v),
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.fiscal_year_help",
            )),
            self.checkbox(
                "nd_marker",
                &crate::localization::text("settings.editor.checkbox.keep_typed_date_comment"),
                d.add_marker,
                cx,
                |this, cx| this.set_natural_dates(cx, |d| d.add_marker = !d.add_marker),
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.date_comment_help",
            )),
            self.checkbox(
                "nd_buttons",
                &crate::localization::text("settings.editor.checkbox.show_refresh_buttons"),
                d.show_refresh_buttons,
                cx,
                |this, cx| {
                    this.set_natural_dates(cx, |d| d.show_refresh_buttons = !d.show_refresh_buttons)
                },
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.date_refresh_buttons_help",
            )),
            self.checkbox(
                "nd_before_run",
                &crate::localization::text("settings.editor.checkbox.refresh_dates_on_run"),
                d.refresh_before_run,
                cx,
                |this, cx| {
                    this.set_natural_dates(cx, |d| d.refresh_before_run = !d.refresh_before_run)
                },
            ),
            self.checkbox(
                "nd_guess",
                &crate::localization::text("settings.editor.checkbox.date_like_column_names"),
                d.guess_from_column_names,
                cx,
                |this, cx| {
                    this.set_natural_dates(cx, |d| {
                        d.guess_from_column_names = !d.guess_from_column_names
                    })
                },
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.date_like_column_names",
            )),
        ])
    }

    /// Change one natural-date preference.
    pub(super) fn set_natural_dates(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut crate::preferences::NaturalDatePreferences),
    ) {
        self.set_editor_field(cx, |e| f(&mut e.natural_dates));
    }

    // ============================================================
    // Editor > Tabs
    // ============================================================

    pub(super) fn render_editor_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.store.tabs();
        let restore_all = matches!(t.restore_policy, crate::preferences::TabRestorePolicy::All);
        let restore_pinned = matches!(
            t.restore_policy,
            crate::preferences::TabRestorePolicy::PinnedOnly
        );
        let restore_none = matches!(t.restore_policy, crate::preferences::TabRestorePolicy::None);
        let disc_close = matches!(
            t.disconnect_policy,
            crate::preferences::DisconnectTabPolicy::CloseTabs
        );
        let disc_clear = matches!(
            t.disconnect_policy,
            crate::preferences::DisconnectTabPolicy::KeepTabsClearResults
        );
        let disc_keep = matches!(
            t.disconnect_policy,
            crate::preferences::DisconnectTabPolicy::KeepTabsKeepResults
        );

        self.pane_container(vec![
            self.heading(&crate::localization::text("settings.editor.heading.tabs")),
            self.intro(&crate::localization::text(
                "settings.editor.intro.tabs_intro",
            )),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.restore_tabs_at_launch",
            )),
            self.radio(
                "tab_restore_all",
                &crate::localization::text("settings.editor.radio.all_tabs"),
                restore_all,
                cx,
                Self::choose_restore_all,
            ),
            self.radio(
                "tab_restore_pinned",
                &crate::localization::text("settings.editor.radio.pinned_tabs_only"),
                restore_pinned,
                cx,
                Self::choose_restore_pinned,
            ),
            self.radio(
                "tab_restore_none",
                &crate::localization::text("settings.editor.radio.none_start_empty_query"),
                restore_none,
                cx,
                Self::choose_restore_none,
            ),
            self.hint(&crate::localization::text(
                "settings.editor.hint.restore_tabs_help",
            )),
            self.section_header(&crate::localization::text(
                "settings.editor.section_header.disconnecting",
            )),
            self.radio(
                "tab_disc_close",
                &crate::localization::text("settings.editor.radio.close_tabs"),
                disc_close,
                cx,
                Self::choose_disconnect_close,
            ),
            self.radio(
                "tab_disc_clear",
                &crate::localization::text("settings.editor.radio.disconnect_clear_results"),
                disc_clear,
                cx,
                Self::choose_disconnect_clear,
            ),
            self.radio(
                "tab_disc_keep",
                &crate::localization::text("settings.editor.radio.keep_tabs_results"),
                disc_keep,
                cx,
                Self::choose_disconnect_keep,
            ),
        ])
    }

    // ============================================================
    // Editor > External Editors
    // ============================================================

    pub(super) fn render_editor_external_editors(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Load external editor preferences from disk (file-backed store, like
        // the macOS version).
        let path = external_editor_prefs_path();
        let prefs = ExternalEditorPreferencesStore::load(&path);
        let selected = prefs.selected_editor;
        let resolved = prefs.selected_resolved_binary_path(|p| std::path::Path::new(p).exists());

        let mut items: Vec<AnyElement> = vec![
            self.heading(&crate::localization::text(
                "settings.editor.heading.external_editors",
            )),
            self.intro(&crate::localization::text(
                "settings.editor.intro.external_editors_intro",
            )),
        ];
        if !crate::distribution::can_launch_user_programs() {
            items.push(crate::app_store_edition_dialog::edition_notice(
                &crate::localization::text("settings.editor.static.app_store_edition_notice"),
            ));
        }
        items.push(self.section_header(&crate::localization::text(
            "settings.editor.section_header.default_editor",
        )));

        items.push(self.enum_select_in(
            "ext_editor_picker",
            selected,
            ExternalEditorId::all().to_vec(),
            |id| {
                if *id == ExternalEditorId::Other {
                    crate::localization::text("settings.editor.external_editors.other")
                } else {
                    ExternalEditorPreset::for_id(*id).display_name.to_string()
                }
            },
            |this, id, window, cx| this.set_ext_editor(id, window, cx),
            window,
            cx,
        ));

        if selected == ExternalEditorId::Other {
            items.push(self.section_header(&crate::localization::text(
                "settings.editor.section_header.name",
            )));
            items.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(self.prefs_input("prefs_ext_name"))
                    .into_any_element(),
            );
        }

        items.push(self.section_header(&crate::localization::text(
            "settings.editor.section_header.binary_path",
        )));
        items.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(8.)))
                .child(div().flex_1().child(self.prefs_input("prefs_ext_path")))
                .into_any_element(),
        );

        if selected != ExternalEditorId::Other {
            let has_override = prefs
                .path_overrides
                .get(selected.as_str())
                .map(|p| !p.is_empty())
                .unwrap_or(false);
            items.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::scaled(8.)))
                    .child(
                        div()
                            .px(px(Theme::scaled(12.)))
                            .py(px(Theme::scaled(4.)))
                            .bg(Theme::panel_bg())
                            .border_1()
                            .border_color(Theme::border())
                            .rounded(px(Theme::scaled(4.)))
                            .cursor_pointer()
                            .text_color(if has_override {
                                Theme::text()
                            } else {
                                Theme::text_dim()
                            })
                            .text_size(px(Theme::font_size()))
                            .id("ext_editor_reset")
                            .child(self.match_text(&crate::localization::text(
                                "settings.editor.match_text.reset_default",
                            )))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(Self::reset_ext_path),
                            ),
                    )
                    .into_any_element(),
            );
        }

        if resolved.is_empty() {
            items.push(self.warning(&crate::localization::text(
                "settings.editor.warning.external_editor_path_missing",
            )));
        } else if !std::path::Path::new(&resolved).exists() {
            items.push(self.warning(&crate::localization::text(
                "settings.editor.warning.path_not_found",
            )));
        }

        if prefs.is_terminal_editor(selected) {
            items.push(self.hint(&crate::localization::text(
                "settings.editor.hint.terminal_editor_note",
            )));
        }

        self.pane_container(items)
    }

    // ============================================================
    // Toggle handlers — Editor
    // ============================================================

    pub(super) fn toggle_show_line_numbers(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.show_line_numbers = !e.show_line_numbers);
    }

    pub(super) fn toggle_show_statement_frame(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.show_statement_frame = !e.show_statement_frame);
    }

    pub(super) fn toggle_show_hover_info(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.show_hover_info = !e.show_hover_info);
    }

    pub(super) fn toggle_show_signature_help(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.show_signature_help = !e.show_signature_help);
    }

    pub(super) fn toggle_show_insert_value_hints(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.show_insert_value_hints = !e.show_insert_value_hints;
        });
    }

    pub(super) fn toggle_bold_keywords(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.syntax_bold.keywords = !e.syntax_bold.keywords);
    }

    pub(super) fn toggle_bold_literals(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.syntax_bold.literals = !e.syntax_bold.literals);
    }

    pub(super) fn toggle_bold_functions(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.syntax_bold.functions = !e.syntax_bold.functions);
    }

    pub(super) fn toggle_bold_system_types(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.syntax_bold.system_types = !e.syntax_bold.system_types
        });
    }

    pub(super) fn toggle_bold_procedures(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.syntax_bold.procedures = !e.syntax_bold.procedures);
    }

    pub(super) fn toggle_bold_schemas(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.syntax_bold.schemas = !e.syntax_bold.schemas);
    }

    pub(super) fn toggle_bold_user_functions(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.syntax_bold.user_functions = !e.syntax_bold.user_functions
        });
    }

    pub(super) fn toggle_bold_tables(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.syntax_bold.tables = !e.syntax_bold.tables);
    }

    pub(super) fn toggle_bold_user_types(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.syntax_bold.user_types = !e.syntax_bold.user_types);
    }

    pub(super) fn toggle_camel_hump_word_movement(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.camel_hump_word_movement = !e.camel_hump_word_movement;
        });
    }

    pub(super) fn toggle_highlight_occurrences(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.highlight_occurrences = !e.highlight_occurrences);
    }

    pub(super) fn toggle_detect_query_placeholders(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.detect_query_placeholders = !e.detect_query_placeholders
        });
    }

    pub(super) fn toggle_vim_mode(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.vim_mode = !e.vim_mode);
    }

    pub(super) fn toggle_auto_spacing(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.auto_spacing = !e.auto_spacing);
    }

    pub(super) fn toggle_automagic_select_commas(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.automagic_select_commas = !e.automagic_select_commas;
        });
    }

    pub(super) fn toggle_automagic_where_connectors(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.automagic_where_connectors = !e.automagic_where_connectors;
        });
    }

    pub(super) fn toggle_automagic_group_by_commas(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.automagic_group_by_commas = !e.automagic_group_by_commas;
        });
    }

    pub(super) fn toggle_automagic_order_by_commas(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.automagic_order_by_commas = !e.automagic_order_by_commas;
        });
    }

    pub(super) fn choose_automagic_trailing(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.automagic_comma_placement = AutomagicCommaPlacement::Trailing;
        });
    }

    pub(super) fn choose_automagic_leading(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.automagic_comma_placement = AutomagicCommaPlacement::Leading;
        });
    }

    pub(super) fn toggle_feedback_full_data_model(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.feedback_include_full_data_model = !e.feedback_include_full_data_model;
        });
    }

    pub(super) fn toggle_feedback_show_tables(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.feedback_show_tables = !e.feedback_show_tables);
    }

    pub(super) fn toggle_feedback_show_views(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.feedback_show_views = !e.feedback_show_views);
    }

    pub(super) fn toggle_feedback_show_procedures(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.feedback_show_procedures = !e.feedback_show_procedures
        });
    }

    pub(super) fn toggle_feedback_show_functions(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.feedback_show_functions = !e.feedback_show_functions
        });
    }

    pub(super) fn toggle_feedback_show_columns(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.feedback_show_columns = !e.feedback_show_columns);
    }

    pub(super) fn toggle_feedback_show_types(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.feedback_show_types = !e.feedback_show_types);
    }

    pub(super) fn choose_tabs_indent(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.indent_style = IndentStyle::Tabs);
    }

    pub(super) fn choose_spaces_indent(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.indent_style = IndentStyle::Spaces);
    }

    pub(super) fn dec_spaces(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.spaces_per_tab_raw = (e.spaces_per_tab_raw - 1).max(1);
        });
    }

    pub(super) fn inc_spaces(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.spaces_per_tab_raw = (e.spaces_per_tab_raw + 1).min(16);
        });
    }

    pub(super) fn choose_alias_caps(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.alias_generation = AliasGeneration::CapitalLettersLowercase;
        });
    }

    pub(super) fn choose_alias_auto(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.alias_generation = AliasGeneration::AutoBasedOnJoin;
        });
    }

    pub(super) fn choose_alias_dns(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.alias_generation = AliasGeneration::DoNotSuggest;
        });
    }

    pub(super) fn choose_alias_add_automatically(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.auto_add_table_alias = true);
    }

    pub(super) fn choose_alias_add_on_second_accept(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.auto_add_table_alias = false);
    }

    pub(super) fn toggle_alias_as_keyword(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.alias_as_keyword = !e.alias_as_keyword);
    }

    pub(super) fn toggle_hump_matching(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.hump_matching = !e.hump_matching);
    }

    pub(super) fn toggle_schema_in_search(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| e.schema_in_search = !e.schema_in_search);
    }

    pub(super) fn choose_restore_all(&mut self, cx: &mut Context<Self>) {
        self.set_tab_field(cx, |t| {
            t.restore_policy = crate::preferences::TabRestorePolicy::All;
        });
    }

    pub(super) fn choose_restore_pinned(&mut self, cx: &mut Context<Self>) {
        self.set_tab_field(cx, |t| {
            t.restore_policy = crate::preferences::TabRestorePolicy::PinnedOnly;
        });
    }

    pub(super) fn choose_restore_none(&mut self, cx: &mut Context<Self>) {
        self.set_tab_field(cx, |t| {
            t.restore_policy = crate::preferences::TabRestorePolicy::None;
        });
    }

    pub(super) fn choose_disconnect_close(&mut self, cx: &mut Context<Self>) {
        self.set_tab_field(cx, |t| {
            t.disconnect_policy = crate::preferences::DisconnectTabPolicy::CloseTabs;
        });
    }

    pub(super) fn choose_disconnect_clear(&mut self, cx: &mut Context<Self>) {
        self.set_tab_field(cx, |t| {
            t.disconnect_policy = crate::preferences::DisconnectTabPolicy::KeepTabsClearResults;
        });
    }

    pub(super) fn choose_disconnect_keep(&mut self, cx: &mut Context<Self>) {
        self.set_tab_field(cx, |t| {
            t.disconnect_policy = crate::preferences::DisconnectTabPolicy::KeepTabsKeepResults;
        });
    }

    pub(super) fn toggle_include_schema(&mut self, cx: &mut Context<Self>) {
        self.set_editor_field(cx, |e| {
            e.include_schema_in_alias = !e.include_schema_in_alias;
        });
    }

    // ---- free-text field change wiring lives in each input's
    // `InputEvent::Change` subscription (see the spec table in
    // `PreferencesWindow::new`). ----

    // ---- External-editor pane actions ----

    pub(super) fn update_ext_prefs<F: FnOnce(&mut ExternalEditorPreferences)>(
        &self,
        cx: &mut Context<Self>,
        f: F,
    ) {
        let path = external_editor_prefs_path();
        let mut prefs = ExternalEditorPreferencesStore::load(&path);
        f(&mut prefs);
        let _ = ExternalEditorPreferencesStore::save(&prefs, &path);
        cx.notify();
    }

    /// Refresh the editor name/path inputs to reflect the currently selected
    /// editor (used after the picker changes selection / reset). Goes through
    /// [`Self::set_input_value`] so the programmatic set does not re-run the
    /// path field's apply logic and re-create the override it just cleared.
    pub(super) fn sync_ext_text_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefs = ExternalEditorPreferencesStore::load(&external_editor_prefs_path());
        self.set_input_value("prefs_ext_name", &prefs.custom_display_name, window, cx);
        let resolved = prefs.selected_resolved_binary_path(|p| std::path::Path::new(p).exists());
        self.set_input_value("prefs_ext_path", &resolved, window, cx);
    }

    pub(super) fn set_ext_editor(
        &mut self,
        id: ExternalEditorId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_ext_prefs(cx, |p| p.selected_editor = id);
        self.sync_ext_text_fields(window, cx);
    }

    pub(super) fn reset_ext_path(
        &mut self,
        _ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_ext_prefs(cx, |p| {
            let key = p.selected_editor.as_str().to_string();
            p.path_overrides.remove(&key);
        });
        self.sync_ext_text_fields(window, cx);
    }
}
