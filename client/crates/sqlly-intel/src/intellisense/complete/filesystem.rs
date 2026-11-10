//! Completion items sourced from the filesystem object model
//! ([`sqlly_lang::fsmodel`] schemas/tables/views/procedures/functions/types).

use sqlly_lang::fsmodel::SqlNodeKind;

use super::context::ClauseContext;
use crate::intellisense::score::{
    push_match, push_match_at, push_match_joined_scored, push_match_joined3,
};
use crate::intellisense::types::{
    CompletionCatalog, CompletionItem, CompletionKind, CompletionSource,
};

pub(crate) fn add_filesystem_items(
    items: &mut Vec<CompletionItem>,
    catalog: &CompletionCatalog,
    prefix: &str,
    previous: &str,
    ctx: ClauseContext,
    strict_scope: bool,
) {
    let mut label_buf = String::new();
    let prefix_has_dot = prefix.contains('.');
    for schema in &catalog.filesystem_schemas {
        push_match(
            items,
            &schema.node.name,
            &schema.node.name,
            CompletionKind::Schema,
            CompletionSource::Filesystem,
            28,
            prefix,
        );
        for table in &schema.tables {
            let base =
                if previous == "FROM" || previous.ends_with("JOIN") || ctx.is_table_expecting() {
                    76
                } else {
                    38
                };
            push_match_joined_scored(
                items,
                &schema.node.name,
                &table.source_node.node.name,
                CompletionKind::Table,
                CompletionSource::Filesystem,
                base,
                prefix,
                &mut label_buf,
            );
            if (previous == "SELECT" || prefix_has_dot || ctx.is_column_expecting())
                && !strict_scope
            {
                for column in &table.columns {
                    if prefix_has_dot {
                        push_match_joined3(
                            items,
                            &schema.node.name,
                            &table.source_node.node.name,
                            &column.node.name,
                            &column.node.name,
                            CompletionKind::Column,
                            CompletionSource::Filesystem,
                            66,
                            prefix,
                            &mut label_buf,
                        );
                    } else {
                        push_match(
                            items,
                            &column.node.name,
                            &column.node.name,
                            CompletionKind::Column,
                            CompletionSource::Filesystem,
                            66,
                            prefix,
                        );
                    }
                }
            }
        }
        for view in &schema.views {
            push_match_joined_scored(
                items,
                &schema.node.name,
                &view.source_node.node.name,
                CompletionKind::Table,
                CompletionSource::Filesystem,
                38,
                prefix,
                &mut label_buf,
            );
        }
        for procedure in &schema.procedures {
            push_match_joined_scored(
                items,
                &schema.node.name,
                &procedure.operation.source_node.node.name,
                CompletionKind::Procedure,
                CompletionSource::Filesystem,
                36,
                prefix,
                &mut label_buf,
            );
            for param in &procedure.operation.input_parameters {
                push_match_at(
                    items,
                    &param.node.name,
                    CompletionKind::Parameter,
                    CompletionSource::Filesystem,
                    62,
                    prefix,
                    &mut label_buf,
                );
            }
        }
        for function in &schema.functions {
            push_match_joined_scored(
                items,
                &schema.node.name,
                &function.operation.source_node.node.name,
                CompletionKind::Function,
                CompletionSource::Filesystem,
                36,
                prefix,
                &mut label_buf,
            );
        }
        for custom_type in &schema.types {
            if custom_type.source_node.node.kind == SqlNodeKind::Type {
                push_match_joined_scored(
                    items,
                    &schema.node.name,
                    &custom_type.source_node.node.name,
                    CompletionKind::Type,
                    CompletionSource::Filesystem,
                    34,
                    prefix,
                    &mut label_buf,
                );
            }
        }
    }
}

/// Finds columns from filesystem tables matching `name`, which may be
/// schema-qualified (`Accounting.JournalLine`) or bare (`JournalLine`).
/// Returns `(column_name, schema_name)` pairs so callers can emit them
/// with the correct source label.
pub(crate) fn find_filesystem_table_columns<'a>(
    catalog: &'a CompletionCatalog,
    name: &str,
) -> Vec<(&'a str, &'a str)> {
    let name = name.trim_matches(|ch| ch == '[' || ch == ']');
    let (schema_filter, table_name) = match name.rsplit_once('.') {
        Some((schema, table)) => (Some(schema), table),
        None => (None, name),
    };

    let mut cols = Vec::new();
    for fs_schema in &catalog.filesystem_schemas {
        if let Some(sf) = schema_filter
            && !fs_schema.node.name.eq_ignore_ascii_case(sf)
        {
            continue;
        }
        for fs_table in &fs_schema.tables {
            if !fs_table
                .source_node
                .node
                .name
                .eq_ignore_ascii_case(table_name)
            {
                continue;
            }
            for column in &fs_table.columns {
                cols.push((column.node.name.as_str(), fs_schema.node.name.as_str()));
            }
        }
        for fs_view in &fs_schema.views {
            if !fs_view
                .source_node
                .node
                .name
                .eq_ignore_ascii_case(table_name)
            {
                continue;
            }
            for column in &fs_view.columns {
                cols.push((column.node.name.as_str(), fs_schema.node.name.as_str()));
            }
        }
    }
    cols
}
