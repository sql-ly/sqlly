//! Explorer "Generate Model": typed data-model code from a table's live
//! column metadata, into a new tab or the clipboard.
//!
//! The metadata comes from the same catalog loader the Table Structure
//! Editor uses (`load_table_shape` is its column/primary-key subset), so the
//! generated types see exactly what the editor shows: engine type,
//! nullability, primary key, identity, defaults. The read runs against the
//! node's own server/database — never whatever connection is active — and
//! nothing here retargets the active connection.

use super::*;
use crate::model_codegen::{ModelLanguage, ModelTable, generate_models};
use crate::schema_tree::{ExplorerActionContext, NodeKind, ScriptDestination};

/// Tab name for generated code: `<stem>.<ext>`.
pub(crate) fn model_tab_name(stem: &str, language: ModelLanguage) -> String {
    let stem = stem.trim();
    let stem = if stem.is_empty() { "model" } else { stem };
    format!("{stem}.{}", language.extension())
}

impl AppView {
    /// Generate one table/view's model.
    pub(super) fn generate_model_for_node(
        &mut self,
        context: &ExplorerActionContext,
        language: ModelLanguage,
        destination: ScriptDestination,
        cx: &mut Context<Self>,
    ) {
        let Some(table) = context.name.clone().filter(|n| !n.is_empty()) else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.models.select_object"),
            );
            cx.notify();
            return;
        };
        let Some((profile, database)) = self.model_target(context, cx) else {
            return;
        };
        let schema = context.schema.clone();
        let services = self.services_for_profile(&profile);
        let qualified = match schema.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => format!("{s}.{table}"),
            None => table.clone(),
        };
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.models.generating",
                &[
                    ("language", language.label()),
                    ("object", qualified.clone()),
                ],
            ),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let hydrated = cx
                .background_executor()
                .spawn(async move { crate::sql_credentials::hydrate_profile(profile) })
                .await;
            let result = match hydrated {
                Ok(profile) => sqlly_services::load_table_shape(
                    &*services.queries,
                    &profile,
                    &database,
                    schema.as_deref(),
                    &table,
                )
                .await
                .map(|shape| (profile.database_type, shape))
                .map_err(|e| e.to_string()),
                Err(reason) => Err(crate::localization::format(
                    "shell.models.credentials_unresolved",
                    &[("error", &crate::localization::message(reason))],
                )),
            };
            let _ = this.update(cx, |app, cx| {
                match result {
                    Ok((db, shape)) => {
                        let model = ModelTable::from_shape(&shape);
                        let code = generate_models(language, db, std::slice::from_ref(&model));
                        app.deliver_generated_model(code, language, &shape.table, destination, cx);
                    }
                    Err(message) => app.set_status(
                        StatusLevel::Error,
                        crate::localization::format(
                            "shell.models.columns_failed",
                            &[("object", &qualified), ("error", &message)],
                        ),
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Generate one file holding a model for every table in the schema
    /// (schema-folder node) or database (database node).
    pub(super) fn generate_models_for_scope(
        &mut self,
        context: &ExplorerActionContext,
        language: ModelLanguage,
        cx: &mut Context<Self>,
    ) {
        let Some((profile, database)) = self.model_target(context, cx) else {
            return;
        };
        let schema = match &context.kind {
            NodeKind::SchemaFolder { schema } => Some(schema.clone()),
            _ => None,
        };
        let services = self.services_for_profile(&profile);
        let scope_label = schema.clone().unwrap_or_else(|| database.clone());
        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.models.generating_scope",
                &[
                    ("language", language.label()),
                    ("scope", scope_label.clone()),
                ],
            ),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let hydrated = cx
                .background_executor()
                .spawn(async move { crate::sql_credentials::hydrate_profile(profile) })
                .await;
            let result: Result<
                (sqlly_lang::sqlmodel::DatabaseType, Vec<ModelTable>, usize),
                String,
            > = match hydrated {
                Ok(profile) => {
                    match services
                        .metadata
                        .list_tables(&profile, &database, schema.as_deref())
                        .await
                    {
                        Ok(tables) => {
                            let mut models = Vec::with_capacity(tables.len());
                            let mut skipped = 0usize;
                            for node in tables {
                                let node_schema = node.schema.clone().or_else(|| schema.clone());
                                match sqlly_services::load_table_shape(
                                    &*services.queries,
                                    &profile,
                                    &database,
                                    node_schema.as_deref(),
                                    &node.name,
                                )
                                .await
                                {
                                    Ok(shape) => models.push(ModelTable::from_shape(&shape)),
                                    Err(err) => {
                                        tracing::warn!(
                                            table = %node.name,
                                            %err,
                                            "skipping table in model generation"
                                        );
                                        skipped += 1;
                                    }
                                }
                            }
                            Ok((profile.database_type, models, skipped))
                        }
                        Err(e) => Err(e.to_string()),
                    }
                }
                Err(reason) => Err(crate::localization::format(
                    "shell.models.scope_credentials_unresolved",
                    &[("error", &crate::localization::message(reason))],
                )),
            };
            let _ = this.update(cx, |app, cx| {
                match result {
                    Ok((_, models, skipped)) if models.is_empty() => {
                        app.set_status(
                            StatusLevel::Warning,
                            if skipped > 0 {
                                crate::localization::format(
                                    "shell.models.none_generated",
                                    &[("skipped", &skipped.to_string())],
                                )
                            } else {
                                crate::localization::format(
                                    "shell.models.no_tables",
                                    &[("scope", &scope_label)],
                                )
                            },
                        );
                    }
                    Ok((db, models, skipped)) => {
                        let count = models.len();
                        let code = generate_models(language, db, &models);
                        let stem = format!("{scope_label}-models");
                        app.deliver_generated_model(
                            code,
                            language,
                            &stem,
                            ScriptDestination::NewQuery,
                            cx,
                        );
                        if skipped > 0 {
                            let count = count.to_string();
                            let skipped = skipped.to_string();
                            app.set_status(
                                StatusLevel::Warning,
                                crate::localization::format(
                                    "shell.models.opened_with_skips",
                                    &[
                                        ("language", language.label()),
                                        ("count", count),
                                        ("skipped", skipped),
                                    ],
                                ),
                            );
                        }
                    }
                    Err(message) => app.set_status(
                        StatusLevel::Error,
                        crate::localization::format(
                            "shell.models.list_failed",
                            &[("scope", &scope_label), ("error", &message)],
                        ),
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The node's profile and database, or a status message and `None`.
    fn model_target(
        &mut self,
        context: &ExplorerActionContext,
        cx: &mut Context<Self>,
    ) -> Option<(ConnectionProfile, String)> {
        let Some(profile) = self
            .resolve_explorer_target(context.server_id.as_ref(), context.database.as_deref(), cx)
            .or_else(|| self.active_profile.clone())
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.models.connect"),
            );
            cx.notify();
            return None;
        };
        let database = context
            .database
            .clone()
            .or_else(|| profile.database.clone())
            .unwrap_or_default();
        Some((profile, database))
    }

    /// Put generated code where the menu asked: a new tab named
    /// `<stem>.<ext>` (a fresh tab, never the focused editor, so no undo
    /// stack is lost) or the clipboard.
    fn deliver_generated_model(
        &mut self,
        code: String,
        language: ModelLanguage,
        stem: &str,
        destination: ScriptDestination,
        cx: &mut Context<Self>,
    ) {
        match destination {
            ScriptDestination::Clipboard => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(code));
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::format(
                        "shell.models.copied",
                        &[("language", language.label())],
                    ),
                );
            }
            ScriptDestination::NewQuery => {
                self.new_tab(cx, Some(code));
                let name = model_tab_name(stem, language);
                let idx = self.active_tab;
                if let Some(tab) = self.tabs.get_mut(idx) {
                    tab.name = name.clone();
                    tab.auto_named = false;
                }
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::format(
                        "shell.models.opened",
                        &[("language", language.label()), ("name", name)],
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_name_uses_language_extension() {
        assert_eq!(
            model_tab_name("Orders", ModelLanguage::TypeScript),
            "Orders.ts"
        );
        assert_eq!(model_tab_name("Orders", ModelLanguage::Zod), "Orders.ts");
        assert_eq!(
            model_tab_name("Orders", ModelLanguage::Prisma),
            "Orders.prisma"
        );
        assert_eq!(
            model_tab_name("dbo-models", ModelLanguage::Go),
            "dbo-models.go"
        );
        assert_eq!(model_tab_name("  ", ModelLanguage::Rust), "model.rs");
    }
}
