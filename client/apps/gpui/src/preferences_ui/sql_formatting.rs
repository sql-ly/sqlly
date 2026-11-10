//! SQL formatting pane: live preview, option controls, and toggles.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

impl PreferencesWindow {
    /// The sample-query dropdown for the formatting preview.
    pub(super) fn sql_preview_sample_select(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title_keys = [
            "settings.sql_formatting.preview_sample.select_joins",
            "settings.sql_formatting.preview_sample.cte_subquery",
            "settings.sql_formatting.preview_sample.insert_update",
            "settings.sql_formatting.preview_sample.create_table",
            "settings.sql_formatting.preview_sample.case_in_list",
        ];
        let options: Vec<DropdownOption> = SQL_PREVIEW_SAMPLES
            .iter()
            .zip(title_keys)
            .enumerate()
            .map(|(i, ((_, sql), title_key))| {
                let sql = *sql;
                (
                    crate::localization::text(title_key),
                    i == self.sql_preview_sample,
                    std::rc::Rc::new(
                        move |this: &mut Self, _window: &mut Window, cx: &mut Context<Self>| {
                            this.sql_preview_sample = i;
                            this.sql_preview_input.update(cx, |editor, cx| {
                                editor.set_text(sql, cx);
                            });
                            cx.notify();
                        },
                    ) as DropdownHandler,
                )
            })
            .collect();
        let label = title_keys
            .get(self.sql_preview_sample)
            .map(|key| crate::localization::text(key))
            .unwrap_or_else(|| {
                crate::localization::text("settings.sql_formatting.preview_sample.custom")
            });
        self.select_dropdown("sql_preview_sample", &label, options, window, cx)
    }

    /// The editable SQL input of the formatting preview.
    pub(super) fn sql_preview_input_box(&self) -> AnyElement {
        div()
            .w_full()
            .h(px(Theme::scaled(140.0)))
            .rounded(px(Theme::scaled(6.0)))
            .border_1()
            .border_color(Theme::border())
            .overflow_hidden()
            .child(self.sql_preview_input.clone())
            .into_any_element()
    }

    /// The live formatted output: the input run through the real engine with
    /// the currently-stored settings, syntax highlighted.
    pub(super) fn sql_preview_output(&self, cx: &Context<Self>) -> AnyElement {
        let s = self.store.sql_formatting();
        let editor_prefs = self.store.editor();
        let input = self.sql_preview_input.read(cx).text().to_string();
        let formatted = crate::sql_formatter::format_sql(
            &input,
            &s,
            Some(crate::sql_formatter::EditorIndent {
                style: editor_prefs.indent_style,
                width: editor_prefs.spaces_per_tab(),
            }),
        );
        let spans = crate::sql_syntax::classify(&formatted);
        let lines: Vec<String> = formatted.split('\n').map(|l| l.to_string()).collect();
        let mut line_start = 0usize;
        let rendered = lines.into_iter().map(move |line| {
            let element = crate::ddl_viewer::render_sql_line(&line, line_start, &spans);
            line_start += line.len() + 1;
            element
        });
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(4.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(self.match_text(&crate::localization::text(
                        "settings.sql_formatting.match_text.formatted_output",
                    ))),
            )
            .child(
                div()
                    .w_full()
                    .rounded(px(Theme::scaled(6.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .bg(Theme::bg())
                    .px(px(Theme::scaled(10.0)))
                    .py(px(Theme::scaled(8.0)))
                    .font_family(Theme::editor_font_family())
                    .overflow_hidden()
                    .children(rendered),
            )
            .into_any_element()
    }

    /// A dropdown over one of the ported SQL-formatting enums, wired to the
    /// store. `apply` names the field; the label comes from the enum's own
    /// `display_title()` via [`SqlOption`].
    pub(super) fn sql_enum<T: SqlOption>(
        &self,
        id: &'static str,
        current: T,
        variants: Vec<T>,
        apply: fn(&mut preferences::SqlFormattingPreferences, T),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.enum_select(
            id,
            current,
            variants,
            |v| v.title(),
            move |this, v, cx| this.set_sql_field(cx, |s| apply(s, v)),
            window,
            cx,
        )
    }

    /// A checkbox wired to one SQL-formatting bool. `toggle` flips the field.
    pub(super) fn sql_check<F>(
        &self,
        id: &'static str,
        label: &str,
        value: bool,
        toggle: F,
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        F: Fn(&mut preferences::SqlFormattingPreferences) + Copy + 'static,
    {
        self.checkbox(id, label, value, cx, move |this, cx| {
            this.set_sql_field(cx, toggle)
        })
    }

    /// One of the ten per-category keyword-casing dropdowns. The ids are a fixed
    /// table because `Select` needs a `&'static str` and the index is runtime.
    pub(super) fn keyword_category_select(
        &self,
        index: usize,
        current: preferences::KeywordCase,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        const IDS: [&str; 10] = [
            "sql_kw_cat_0",
            "sql_kw_cat_1",
            "sql_kw_cat_2",
            "sql_kw_cat_3",
            "sql_kw_cat_4",
            "sql_kw_cat_5",
            "sql_kw_cat_6",
            "sql_kw_cat_7",
            "sql_kw_cat_8",
            "sql_kw_cat_9",
        ];
        self.enum_select(
            IDS[index],
            current,
            preferences::KeywordCase::all().to_vec(),
            |v| v.display_title().to_string(),
            move |this, v, cx| {
                this.set_sql_field(cx, |s| s.keyword_group_casing.set_category(index, v))
            },
            window,
            cx,
        )
    }

    // ============================================================
    // Editor > Formatting (SQL formatting; formerly its own SQL folder)
    // ============================================================

    pub(super) fn render_sql_formatting(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = self.store.sql_formatting();
        let mut items = vec![
            self.heading(&crate::localization::text(
                "settings.sql_formatting.heading.sql_formatting",
            )),
            self.intro(&crate::localization::text(
                "settings.sql_formatting.intro.pane_intro",
            )),
            self.sql_check(
                "sql_fmt_as_you_go",
                &crate::localization::text("settings.sql_formatting.sql_check.format_as_you_go"),
                s.format_as_you_go,
                |s| s.format_as_you_go = !s.format_as_you_go,
                cx,
            ),
            self.hint(&crate::localization::text(
                "settings.sql_formatting.hint.format_as_you_go_help",
            )),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.try_it",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.sample"),
                self.sql_preview_sample_select(window, cx),
            ),
            self.hint(&crate::localization::text(
                "settings.sql_formatting.hint.preview_sample",
            )),
            self.sql_preview_input_box(),
            self.sql_preview_output(cx),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.preset",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.preset"),
                self.enum_select_in(
                    "sql_preset",
                    s.preset,
                    Preset::all().to_vec(),
                    |p| p.display_title().to_string(),
                    |this, p, window, cx| this.apply_sql_preset(p, window, cx),
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.sql_formatting.hint.preset_overrides_settings",
            )),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.dialect",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.sql_dialect"),
                self.sql_enum(
                    "sql_dialect",
                    s.dialect,
                    SqlDialect::all().to_vec(),
                    |s, v| s.dialect = v,
                    window,
                    cx,
                ),
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.keyword_casing",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.case"),
                self.sql_enum(
                    "sql_kw_case",
                    s.keyword_case,
                    KeywordCase::all().to_vec(),
                    |s, v| s.keyword_case = v,
                    window,
                    cx,
                ),
            ),
            self.sql_check(
                "sql_kw_group",
                &crate::localization::text("settings.sql_formatting.sql_check.per_category_casing"),
                s.keyword_group_casing.enabled,
                |s| s.keyword_group_casing.enabled = !s.keyword_group_casing.enabled,
                cx,
            ),
        ];

        // The ten per-category casing dropdowns only exist when the master is
        // on. Showing them disabled would put ten dead controls on screen; the
        // conditional teaches the dependency instead.
        if s.keyword_group_casing.enabled {
            for (i, label) in KeywordGroupCasing::CATEGORY_LABELS.iter().enumerate() {
                // `category` is `Option` for out-of-range indices; iterating the
                // label table means `i` is always in range.
                if let Some(current) = s.keyword_group_casing.category(i) {
                    items.push(
                        self.labeled_row(
                            label,
                            self.keyword_category_select(i, current, window, cx),
                        ),
                    );
                }
            }
        }

        items.extend(vec![
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.identifier_formatting",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.identifier_case"),
                self.sql_enum(
                    "sql_id_case",
                    s.identifier_case,
                    IdentifierCase::all().to_vec(),
                    |s, v| s.identifier_case = v,
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.quoting"),
                self.sql_enum(
                    "sql_id_quote",
                    s.identifier_quoting,
                    IdentifierQuoting::all().to_vec(),
                    |s, v| s.identifier_quoting = v,
                    window,
                    cx,
                ),
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.clause_layout",
            )),
            self.labeled_row(
                &crate::localization::text(
                    "settings.sql_formatting.labeled_row.major_clause_placement",
                ),
                self.sql_enum(
                    "sql_clause_place",
                    s.major_clause_placement,
                    MajorClausePlacement::all().to_vec(),
                    |s, v| s.major_clause_placement = v,
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.table_after_from"),
                self.sql_enum(
                    "sql_from_table_place",
                    s.from_table_placement,
                    ClauseBodyPlacement::all().to_vec(),
                    |s, v| s.from_table_placement = v,
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text(
                    "settings.sql_formatting.labeled_row.first_condition_placement",
                ),
                self.sql_enum(
                    "sql_where_cond_place",
                    s.where_condition_placement,
                    ClauseBodyPlacement::all().to_vec(),
                    |s, v| s.where_condition_placement = v,
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.sql_formatting.hint.new_line_indented_placement",
            )),
            self.sql_check(
                "sql_break_select",
                &crate::localization::text("settings.sql_formatting.sql_check.break_after_select"),
                s.break_after_select,
                |s| s.break_after_select = !s.break_after_select,
                cx,
            ),
            self.sql_check(
                "sql_blank_clauses",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.blank_line_before_clauses",
                ),
                s.blank_line_before_major_clauses,
                |s| s.blank_line_before_major_clauses = !s.blank_line_before_major_clauses,
                cx,
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.select_list",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.column_layout"),
                self.sql_enum(
                    "sql_select_layout",
                    s.select_column_layout,
                    SelectListStyle::all().to_vec(),
                    |s, v| s.select_column_layout = v,
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text(
                    "settings.sql_formatting.labeled_row.inline_column_threshold",
                ),
                self.stepper(
                    "sql_inline_col_dec",
                    "sql_inline_col_inc",
                    s.inline_column_threshold(),
                    cx,
                    |this, cx| {
                        this.set_sql_field(cx, |s| {
                            s.inline_column_threshold_raw = s.inline_column_threshold() - 1
                        })
                    },
                    |this, cx| {
                        this.set_sql_field(cx, |s| {
                            s.inline_column_threshold_raw = s.inline_column_threshold() + 1
                        })
                    },
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.sql_formatting.hint.inline_column_threshold_range",
            )),
            self.sql_check(
                "sql_align_aliases",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.align_column_aliases",
                ),
                s.align_column_aliases,
                |s| s.align_column_aliases = !s.align_column_aliases,
                cx,
            ),
            self.labeled_row(
                &crate::localization::text(
                    "settings.sql_formatting.labeled_row.group_by_order_by_layout",
                ),
                self.sql_enum(
                    "sql_order_group_layout",
                    s.order_group_layout,
                    OrderGroupLayout::all().to_vec(),
                    |s, v| s.order_group_layout = v,
                    window,
                    cx,
                ),
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.comma_style",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.style"),
                self.sql_enum(
                    "sql_comma_style",
                    s.comma_style,
                    CommaStyle::all().to_vec(),
                    |s, v| s.comma_style = v,
                    window,
                    cx,
                ),
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.boolean_conditions",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.placement"),
                self.sql_enum(
                    "sql_bool_place",
                    s.boolean_operator_placement,
                    BooleanOperatorPlacement::all().to_vec(),
                    |s, v| s.boolean_operator_placement = v,
                    window,
                    cx,
                ),
            ),
            self.sql_check(
                "sql_bool_paren",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.parenthesize_mixed_boolean",
                ),
                s.parenthesize_mixed_boolean_logic,
                |s| s.parenthesize_mixed_boolean_logic = !s.parenthesize_mixed_boolean_logic,
                cx,
            ),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.list_layout"),
                self.sql_enum(
                    "sql_in_list",
                    s.in_list_style,
                    InListStyle::all().to_vec(),
                    |s, v| s.in_list_style = v,
                    window,
                    cx,
                ),
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.case_expressions",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.layout"),
                self.sql_enum(
                    "sql_case_style",
                    s.case_expression_style,
                    CaseExpressionStyle::all().to_vec(),
                    |s, v| s.case_expression_style = v,
                    window,
                    cx,
                ),
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.operators",
            )),
            self.sql_check(
                "sql_spaces_op",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.spaces_around_operators_etc",
                ),
                s.spaces_around_operators,
                |s| s.spaces_around_operators = !s.spaces_around_operators,
                cx,
            ),
            self.sql_check(
                "sql_align_op",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.align_comparison_operators",
                ),
                s.align_comparison_operators,
                |s| s.align_comparison_operators = !s.align_comparison_operators,
                cx,
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.joins",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.layout_2"),
                self.sql_enum(
                    "sql_join_layout",
                    s.join_layout,
                    JoinLayout::all().to_vec(),
                    |s, v| s.join_layout = v,
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.clause_placement"),
                self.sql_enum(
                    "sql_on_place",
                    s.on_clause_placement,
                    OnClausePlacement::all().to_vec(),
                    |s, v| s.on_clause_placement = v,
                    window,
                    cx,
                ),
            ),
            self.sql_check(
                "sql_multi_on",
                &crate::localization::text("settings.sql_formatting.sql_check.multi_condition_on"),
                s.multi_condition_on,
                |s| s.multi_condition_on = !s.multi_condition_on,
                cx,
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.functions",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.case_2"),
                self.sql_enum(
                    "sql_func_case",
                    s.function_case,
                    FunctionCase::all().to_vec(),
                    |s, v| s.function_case = v,
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.argument_style"),
                self.sql_enum(
                    "sql_func_args",
                    s.function_arg_style,
                    FunctionArgStyle::all().to_vec(),
                    |s, v| s.function_arg_style = v,
                    window,
                    cx,
                ),
            ),
            self.sql_check(
                "sql_func_space",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.space_before_function_parens",
                ),
                s.space_before_function_parentheses,
                |s| s.space_before_function_parentheses = !s.space_before_function_parentheses,
                cx,
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.subqueries_ctes",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.subquery_style"),
                self.sql_enum(
                    "sql_sub_style",
                    s.subquery_style,
                    SubqueryStyle::all().to_vec(),
                    |s, v| s.subquery_style = v,
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.cte_style"),
                self.sql_enum(
                    "sql_cte_style",
                    s.cte_style,
                    CteStyle::all().to_vec(),
                    |s, v| s.cte_style = v,
                    window,
                    cx,
                ),
            ),
            self.sql_check(
                "sql_cte_newline",
                &crate::localization::text("settings.sql_formatting.sql_check.new_line_after_with"),
                s.new_line_after_with,
                |s| s.new_line_after_with = !s.new_line_after_with,
                cx,
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.dml",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.style_2"),
                self.sql_enum(
                    "sql_dml_style",
                    s.dml_style,
                    DmlStyle::all().to_vec(),
                    |s, v| s.dml_style = v,
                    window,
                    cx,
                ),
            ),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.values_style"),
                self.sql_enum(
                    "sql_values_style",
                    s.values_style,
                    ValuesStyle::all().to_vec(),
                    |s, v| s.values_style = v,
                    window,
                    cx,
                ),
            ),
            self.sql_check(
                "sql_dml_one_col",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.insert_one_column_per_line",
                ),
                s.insert_one_column_per_line,
                |s| s.insert_one_column_per_line = !s.insert_one_column_per_line,
                cx,
            ),
            self.sql_check(
                "sql_dml_align_values",
                &crate::localization::text("settings.sql_formatting.sql_check.align_values_rows"),
                s.align_values_rows,
                |s| s.align_values_rows = !s.align_values_rows,
                cx,
            ),
            self.sql_check(
                "sql_dml_align_set",
                &crate::localization::text("settings.sql_formatting.sql_check.align_set_operators"),
                s.align_set_operators,
                |s| s.align_set_operators = !s.align_set_operators,
                cx,
            ),
            self.sql_check(
                "sql_dml_merge",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.merge_section_separation",
                ),
                s.merge_section_separation,
                |s| s.merge_section_separation = !s.merge_section_separation,
                cx,
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.ddl",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.style_3"),
                self.sql_enum(
                    "sql_ddl_style",
                    s.ddl_style,
                    DdlStyle::all().to_vec(),
                    |s, v| s.ddl_style = v,
                    window,
                    cx,
                ),
            ),
            self.sql_check(
                "sql_ddl_align_dt",
                &crate::localization::text("settings.sql_formatting.sql_check.align_data_types"),
                s.align_data_types,
                |s| s.align_data_types = !s.align_data_types,
                cx,
            ),
            self.sql_check(
                "sql_ddl_align_null",
                &crate::localization::text("settings.sql_formatting.sql_check.align_nullability"),
                s.align_nullability,
                |s| s.align_nullability = !s.align_nullability,
                cx,
            ),
            self.sql_check(
                "sql_ddl_constr_after",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.constraints_after_columns",
                ),
                s.constraints_after_columns,
                |s| s.constraints_after_columns = !s.constraints_after_columns,
                cx,
            ),
            self.sql_check(
                "sql_ddl_multi_idx",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.multiline_index_columns",
                ),
                s.multiline_index_columns,
                |s| s.multiline_index_columns = !s.multiline_index_columns,
                cx,
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.comments",
            )),
            self.labeled_row(
                &crate::localization::text("settings.sql_formatting.labeled_row.style_4"),
                self.sql_enum(
                    "sql_comment_style",
                    s.comment_style,
                    CommentStyle::all().to_vec(),
                    |s, v| s.comment_style = v,
                    window,
                    cx,
                ),
            ),
            self.sql_check(
                "sql_comment_inline",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.keep_inline_comments_inline",
                ),
                s.keep_inline_comments_inline,
                |s| s.keep_inline_comments_inline = !s.keep_inline_comments_inline,
                cx,
            ),
            self.sql_check(
                "sql_comment_move",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.move_trailing_comments",
                ),
                s.move_trailing_comments_with_expression,
                |s| {
                    s.move_trailing_comments_with_expression =
                        !s.move_trailing_comments_with_expression
                },
                cx,
            ),
            self.sql_check(
                "sql_comment_block",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.format_inside_block_comments",
                ),
                s.format_inside_block_comments,
                |s| s.format_inside_block_comments = !s.format_inside_block_comments,
                cx,
            ),
            self.sql_check(
                "sql_comment_control",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.respect_control_comments",
                ),
                s.respect_control_comments,
                |s| s.respect_control_comments = !s.respect_control_comments,
                cx,
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.format_triggers",
            )),
            self.sql_check(
                "sql_fmt_save",
                &crate::localization::text("settings.sql_formatting.sql_check.format_on_save"),
                s.format_on_save,
                |s| s.format_on_save = !s.format_on_save,
                cx,
            ),
            self.sql_check(
                "sql_fmt_paste",
                &crate::localization::text("settings.sql_formatting.sql_check.format_on_paste"),
                s.format_on_paste,
                |s| s.format_on_paste = !s.format_on_paste,
                cx,
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.indentation",
            )),
            self.sql_check(
                "sql_fmt_use_editor",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.use_editor_indentation",
                ),
                s.use_editor_indentation,
                |s| s.use_editor_indentation = !s.use_editor_indentation,
                cx,
            ),
        ]);

        // Indentation detail only applies when the formatter is not deferring to
        // the editor's own settings.
        if !s.use_editor_indentation {
            items.extend(vec![
                self.labeled_row(
                    &crate::localization::text("settings.sql_formatting.labeled_row.indent_style"),
                    self.sql_enum(
                        "sql_indent_style",
                        s.indent_style,
                        IndentStyle::all().to_vec(),
                        |s, v| s.indent_style = v,
                        window,
                        cx,
                    ),
                ),
                self.labeled_row(
                    &crate::localization::text("settings.sql_formatting.labeled_row.indent_width"),
                    self.stepper(
                        "sql_indent_w_dec",
                        "sql_indent_w_inc",
                        s.indent_width(),
                        cx,
                        |this, cx| {
                            this.set_sql_field(cx, |s| s.indent_width_raw = s.indent_width() - 1)
                        },
                        |this, cx| {
                            this.set_sql_field(cx, |s| s.indent_width_raw = s.indent_width() + 1)
                        },
                    ),
                ),
                self.labeled_row(
                    &crate::localization::text(
                        "settings.sql_formatting.labeled_row.continuation_indent",
                    ),
                    self.stepper(
                        "sql_cont_w_dec",
                        "sql_cont_w_inc",
                        s.continuation_indent_size(),
                        cx,
                        |this, cx| {
                            this.set_sql_field(cx, |s| {
                                s.continuation_indent_size_raw = s.continuation_indent_size() - 1
                            })
                        },
                        |this, cx| {
                            this.set_sql_field(cx, |s| {
                                s.continuation_indent_size_raw = s.continuation_indent_size() + 1
                            })
                        },
                    ),
                ),
            ]);
        }

        items.extend(vec![
            self.sql_check(
                "sql_indent_nested",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.indent_nested_queries",
                ),
                s.indent_nested_queries,
                |s| s.indent_nested_queries = !s.indent_nested_queries,
                cx,
            ),
            self.sql_check(
                "sql_indent_join_on",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.indent_join_on_conditions",
                ),
                s.indent_join_on,
                |s| s.indent_join_on = !s.indent_join_on,
                cx,
            ),
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.line_length",
            )),
            self.input_row(
                &crate::localization::text("settings.sql_formatting.input_row.max_line_length"),
                "sql_line_len",
            ),
            // The clamp is applied on read, so a raw value outside the range is
            // kept but not honored. Saying which number will actually be used
            // beats letting the field show one thing and the formatter use
            // another.
            if s.max_line_length_raw != s.max_line_length() {
                self.warning(&crate::localization::format(
                    "settings.sql_formatting.message.max_line_length_out_of_range",
                    &[("value", &(s.max_line_length()).to_string())],
                ))
            } else {
                self.hint(&crate::localization::text(
                    "settings.sql_formatting.hint.max_line_length_range",
                ))
            },
            self.sql_check(
                "sql_line_wrap",
                &crate::localization::text("settings.sql_formatting.sql_check.wrap_long_lines"),
                s.wrap_long_lines,
                |s| s.wrap_long_lines = !s.wrap_long_lines,
                cx,
            ),
            self.sql_check(
                "sql_short_one_line",
                &crate::localization::text(
                    "settings.sql_formatting.sql_check.short_queries_one_line",
                ),
                s.keep_short_queries_on_one_line,
                |s| s.keep_short_queries_on_one_line = !s.keep_short_queries_on_one_line,
                cx,
            ),
        ]);

        if s.keep_short_queries_on_one_line {
            items.push(self.input_row(
                &crate::localization::text(
                    "settings.sql_formatting.input_row.short_query_max_length",
                ),
                "sql_short_len",
            ));
            if s.short_query_max_length_raw != s.short_query_max_length() {
                items.push(self.warning(&crate::localization::format(
                    "settings.sql_formatting.message.short_query_length_range",
                    &[("value", &(s.short_query_max_length()).to_string())],
                )));
            }
        }

        items.extend(vec![
            self.section_header(&crate::localization::text(
                "settings.sql_formatting.section_header.statements",
            )),
            self.labeled_row(
                &crate::localization::text(
                    "settings.sql_formatting.labeled_row.semicolon_placement",
                ),
                self.sql_enum(
                    "sql_semicolon",
                    s.semicolon_style,
                    SemicolonStyle::all().to_vec(),
                    |s, v| s.semicolon_style = v,
                    window,
                    cx,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.sql_formatting.hint.semicolon_move_only",
            )),
            self.labeled_row(
                &crate::localization::text(
                    "settings.sql_formatting.labeled_row.max_consecutive_blank_lines",
                ),
                self.stepper(
                    "sql_blank_dec",
                    "sql_blank_inc",
                    s.max_consecutive_blank_lines(),
                    cx,
                    |this, cx| {
                        this.set_sql_field(cx, |s| {
                            s.max_consecutive_blank_lines_raw = s.max_consecutive_blank_lines() - 1
                        })
                    },
                    |this, cx| {
                        this.set_sql_field(cx, |s| {
                            s.max_consecutive_blank_lines_raw = s.max_consecutive_blank_lines() + 1
                        })
                    },
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.sql_formatting.hint.max_blank_lines_range",
            )),
        ]);

        self.pane_container(items)
    }
}
