//! Data Model pane.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

impl PreferencesWindow {
    // ============================================================
    // Data Model > Server Explorer
    // ============================================================

    pub(super) fn set_data_model_field<F>(&self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(&mut preferences::DataModelPreferences),
    {
        let mut p = self.store.data_model();
        f(&mut p);
        self.store.set_data_model(p);
        cx.notify();
    }

    pub(super) fn dec_data_type_width(&mut self, cx: &mut Context<Self>) {
        self.set_data_model_field(cx, |p| {
            p.data_type_format.width_chars = p.data_type_format.width_chars.saturating_sub(1);
        });
    }

    pub(super) fn inc_data_type_width(&mut self, cx: &mut Context<Self>) {
        self.set_data_model_field(cx, |p| {
            p.data_type_format.width_chars = (p.data_type_format.width_chars + 1).min(80);
        });
    }

    pub(super) fn dec_nullability_width(&mut self, cx: &mut Context<Self>) {
        self.set_data_model_field(cx, |p| {
            p.nullability_format.width_chars = p.nullability_format.width_chars.saturating_sub(1);
        });
    }

    pub(super) fn inc_nullability_width(&mut self, cx: &mut Context<Self>) {
        self.set_data_model_field(cx, |p| {
            p.nullability_format.width_chars = (p.nullability_format.width_chars + 1).min(80);
        });
    }

    pub(super) fn render_data_model(&self, cx: &mut Context<Self>) -> AnyElement {
        use preferences::{
            GeneralSchemaPlacement, MetadataAlignment, SchemaDisplayMode, SystemDatabasePlacement,
            SystemSchemaPlacement,
        };
        let p = self.store.data_model();
        let schema_folders = p.schema_display == SchemaDisplayMode::SchemaFolders;
        let flat = p.schema_display == SchemaDisplayMode::FlatPrefixed;

        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.data_model.heading.server_explorer",
            )),
            self.intro(&crate::localization::text(
                "settings.data_model.intro.server_explorer",
            )),
            self.section_header(&crate::localization::text(
                "settings.data_model.section_header.organization",
            )),
            self.label_row(&crate::localization::text(
                "settings.data_model.label_row.schema_display",
            )),
            self.radio(
                "dm_schema_folders",
                &crate::localization::text("settings.data_model.radio.schema_folders"),
                schema_folders,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.schema_display = preferences::SchemaDisplayMode::SchemaFolders;
                    });
                },
            ),
            self.hint(&crate::localization::text(
                "settings.data_model.hint.schema_folders_path",
            )),
            self.radio(
                "dm_schema_flat",
                &crate::localization::text("settings.data_model.radio.flat_prefixed"),
                flat,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.schema_display = preferences::SchemaDisplayMode::FlatPrefixed;
                    });
                },
            ),
            self.hint(&crate::localization::text(
                "settings.data_model.hint.flat_prefixed_path",
            )),
            self.label_row(&crate::localization::text(
                "settings.data_model.label_row.line_up_object_metadata",
            )),
            self.checkbox(
                "dm_align",
                &crate::localization::text("settings.data_model.checkbox.align_metadata"),
                p.align_metadata,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| p.align_metadata = !p.align_metadata);
                },
            ),
            self.checkbox(
                "dm_align_tables",
                &crate::localization::text("settings.data_model.checkbox.table_columns"),
                p.align_table_columns,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.align_table_columns = !p.align_table_columns;
                    });
                },
            ),
            self.checkbox(
                "dm_align_views",
                &crate::localization::text("settings.data_model.checkbox.view_columns"),
                p.align_view_columns,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| p.align_view_columns = !p.align_view_columns);
                },
            ),
            self.checkbox(
                "dm_align_procs",
                &crate::localization::text(
                    "settings.data_model.checkbox.stored_procedure_parameters",
                ),
                p.align_procedure_parameters,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.align_procedure_parameters = !p.align_procedure_parameters;
                    });
                },
            ),
            self.checkbox(
                "dm_align_fns",
                &crate::localization::text("settings.data_model.checkbox.function_parameters"),
                p.align_function_parameters,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.align_function_parameters = !p.align_function_parameters;
                    });
                },
            ),
            self.checkbox(
                "dm_align_types",
                &crate::localization::text("settings.data_model.checkbox.type_columns"),
                p.align_type_columns,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| p.align_type_columns = !p.align_type_columns);
                },
            ),
            self.label_row(&crate::localization::text(
                "settings.data_model.label_row.navigation",
            )),
            self.checkbox(
                "dm_auto_nav",
                &crate::localization::text("settings.data_model.checkbox.auto_navigate_schema"),
                p.auto_navigate_into_view,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.auto_navigate_into_view = !p.auto_navigate_into_view;
                    });
                },
            ),
            self.hint(&crate::localization::text(
                "settings.data_model.hint.auto_navigate",
            )),
            self.label_row(&crate::localization::text(
                "settings.data_model.label_row.system_databases",
            )),
            self.radio(
                "dm_sys_folder",
                &crate::localization::text("settings.data_model.radio.system_databases_folder"),
                p.system_databases == SystemDatabasePlacement::Folder,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.system_databases = preferences::SystemDatabasePlacement::Folder;
                    });
                },
            ),
            self.radio(
                "dm_sys_inline",
                &crate::localization::text("settings.data_model.radio.show_inline_user_databases"),
                p.system_databases == SystemDatabasePlacement::Inline,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.system_databases = preferences::SystemDatabasePlacement::Inline;
                    });
                },
            ),
            self.radio(
                "dm_sys_hidden",
                &crate::localization::text("settings.data_model.radio.hidden"),
                p.system_databases == SystemDatabasePlacement::Hidden,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.system_databases = preferences::SystemDatabasePlacement::Hidden;
                    });
                },
            ),
            self.hint(&crate::localization::text(
                "settings.data_model.hint.system_databases",
            )),
            self.label_row(&crate::localization::text(
                "settings.data_model.label_row.system_schemas",
            )),
            self.radio(
                "dm_sysch_folder",
                &crate::localization::text("settings.data_model.radio.system_schemas_folder"),
                p.system_schemas == SystemSchemaPlacement::Folder,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.system_schemas = preferences::SystemSchemaPlacement::Folder;
                    });
                },
            ),
            self.radio(
                "dm_sysch_hidden",
                &crate::localization::text("settings.data_model.radio.hidden_2"),
                p.system_schemas == SystemSchemaPlacement::Hidden,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.system_schemas = preferences::SystemSchemaPlacement::Hidden;
                    });
                },
            ),
            self.radio(
                "dm_sysch_nonempty",
                &crate::localization::text("settings.data_model.radio.only_show_not_empty"),
                p.system_schemas == SystemSchemaPlacement::OnlyIfNotEmpty,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.system_schemas = preferences::SystemSchemaPlacement::OnlyIfNotEmpty;
                    });
                },
            ),
            self.hint(&crate::localization::text(
                "settings.data_model.hint.system_schemas",
            )),
            self.label_row(&crate::localization::text(
                "settings.data_model.label_row.general_schemas",
            )),
            self.radio(
                "dm_gsch_show",
                &crate::localization::text("settings.data_model.radio.show_all_schemas"),
                p.general_schemas == GeneralSchemaPlacement::Show,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.general_schemas = preferences::GeneralSchemaPlacement::Show;
                    });
                },
            ),
            self.radio(
                "dm_gsch_nonempty",
                &crate::localization::text("settings.data_model.radio.show_not_empty"),
                p.general_schemas == GeneralSchemaPlacement::ShowIfNotEmpty,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.general_schemas = preferences::GeneralSchemaPlacement::ShowIfNotEmpty;
                    });
                },
            ),
            self.radio(
                "dm_gsch_emptyfolder",
                &crate::localization::text("settings.data_model.radio.empty_schemas_folder"),
                p.general_schemas == GeneralSchemaPlacement::ShowWithEmptyFolder,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.general_schemas =
                            preferences::GeneralSchemaPlacement::ShowWithEmptyFolder;
                    });
                },
            ),
            self.section_header(&crate::localization::text(
                "settings.data_model.section_header.metadata_columns",
            )),
            self.label_row(&crate::localization::text(
                "settings.data_model.label_row.data_type",
            )),
            self.radio(
                "dm_dt_left",
                &crate::localization::text("settings.data_model.radio.align_left"),
                p.data_type_format.alignment == MetadataAlignment::Left,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.data_type_format.alignment = preferences::MetadataAlignment::Left;
                    });
                },
            ),
            self.radio(
                "dm_dt_right",
                &crate::localization::text("settings.data_model.radio.align_right"),
                p.data_type_format.alignment == MetadataAlignment::Right,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.data_type_format.alignment = preferences::MetadataAlignment::Right;
                    });
                },
            ),
            self.labeled_row(
                &crate::localization::text("settings.data_model.labeled_row.width_characters"),
                self.stepper(
                    "dm_dt_w_dec",
                    "dm_dt_w_inc",
                    p.data_type_format.width_chars as i32,
                    cx,
                    Self::dec_data_type_width,
                    Self::inc_data_type_width,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.data_model.hint.data_type_width_auto",
            )),
            self.label_row(&crate::localization::text(
                "settings.data_model.label_row.nullability",
            )),
            self.radio(
                "dm_null_left",
                &crate::localization::text("settings.data_model.radio.align_left_2"),
                p.nullability_format.alignment == MetadataAlignment::Left,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.nullability_format.alignment = preferences::MetadataAlignment::Left;
                    });
                },
            ),
            self.radio(
                "dm_null_right",
                &crate::localization::text("settings.data_model.radio.align_right_2"),
                p.nullability_format.alignment == MetadataAlignment::Right,
                cx,
                |this, cx| {
                    this.set_data_model_field(cx, |p| {
                        p.nullability_format.alignment = preferences::MetadataAlignment::Right;
                    });
                },
            ),
            self.labeled_row(
                &crate::localization::text("settings.data_model.labeled_row.width_characters_2"),
                self.stepper(
                    "dm_null_w_dec",
                    "dm_null_w_inc",
                    p.nullability_format.width_chars as i32,
                    cx,
                    Self::dec_nullability_width,
                    Self::inc_nullability_width,
                ),
            ),
            self.hint(&crate::localization::text(
                "settings.data_model.hint.nullability_width_auto",
            )),
        ])
    }
}
