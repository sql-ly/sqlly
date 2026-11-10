use crate::agent::connections::{
    apply_database_override, effective_database, load_store, resolve_connection, store_path,
};
use crate::agent::format::{self, OutputFormat, ResultSet};
use crate::agent::{CliError, ExitCode};
use serde_json::{Value, json};
use sqlly_core::ObjectKind;
use sqlly_services::metadata::MetadataNode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum SchemaKindFilter {
    #[default]
    All,
    Tables,
    Views,
    Procedures,
    Functions,
}

#[derive(Debug, Clone)]
pub struct ListArgs {
    pub connection: String,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub kind: SchemaKindFilter,
    pub format: OutputFormat,
}

#[derive(Debug, Clone)]
pub struct DescribeArgs {
    pub connection: String,
    pub object: String,
    pub database: Option<String>,
    pub format: OutputFormat,
}

pub async fn list(args: ListArgs, json: bool) -> ExitCode {
    let result = async {
        let store = load_store(&store_path())?;
        let mut profile = resolve_connection(&store, &args.connection)?;
        profile = apply_database_override(profile, args.database);
        let database = effective_database(&profile)?;
        let services = sqlly_services::Services::new();
        let schema = args.schema.as_deref();
        let mut rows = Vec::new();

        if matches!(args.kind, SchemaKindFilter::All | SchemaKindFilter::Tables) {
            rows.extend(nodes_to_rows(
                services
                    .metadata
                    .list_tables(&profile, &database, schema)
                    .await?,
                "table",
            ));
        }
        if matches!(args.kind, SchemaKindFilter::All | SchemaKindFilter::Views) {
            rows.extend(nodes_to_rows(
                services
                    .metadata
                    .list_views(&profile, &database, schema)
                    .await?,
                "view",
            ));
        }
        if matches!(
            args.kind,
            SchemaKindFilter::All | SchemaKindFilter::Procedures | SchemaKindFilter::Functions
        ) {
            let objects = services
                .metadata
                .list_programmable_objects(&profile, &database, schema)
                .await?;
            for node in objects {
                let kind = programmable_kind(&node.kind);
                if args.kind == SchemaKindFilter::All
                    || (args.kind == SchemaKindFilter::Procedures && kind == "procedure")
                    || (args.kind == SchemaKindFilter::Functions && kind == "function")
                {
                    rows.push(object_row(&node, kind));
                }
            }
        }

        rows.sort_by(|a, b| {
            (a.0.as_str(), a.1.as_str(), a.2.as_str()).cmp(&(
                b.0.as_str(),
                b.1.as_str(),
                b.2.as_str(),
            ))
        });

        let rendered = render_object_rows(&rows, args.format);
        let data = json!({ "objects": rows_to_json(&rows) });
        Ok::<_, CliError>((rendered, data))
    }
    .await;

    match result {
        Ok((text, data)) => crate::agent::emit(json, Some(text), Ok(data)),
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

pub async fn describe(args: DescribeArgs, json: bool) -> ExitCode {
    let result = async {
        let store = load_store(&store_path())?;
        let mut profile = resolve_connection(&store, &args.connection)?;
        profile = apply_database_override(profile, args.database);
        let database = effective_database(&profile)?;
        let (schema, object) = split_object(&args.object)?;
        let services = sqlly_services::Services::new();

        let columns = services
            .metadata
            .list_columns(&profile, &database, &schema, &object)
            .await?;

        let primary_keys = services
            .metadata
            .list_primary_keys(&profile, &database)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|pk| pk.table == object && pk.schema == schema)
            .collect::<Vec<_>>();

        let foreign_keys = services
            .metadata
            .list_foreign_keys(&profile, &database)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|fk| fk.from_table == object && fk.from_schema == schema)
            .collect::<Vec<_>>();

        let indexes = services
            .metadata
            .list_indexes(&profile, &database, &schema, &object)
            .await
            .unwrap_or_default();

        let payload = json!({
            "object": format!("{schema}.{object}"),
            "columns": columns.iter().map(metadata_node_json).collect::<Vec<_>>(),
            "primary_keys": primary_keys.iter().map(|pk| json!({
                "schema": pk.schema,
                "table": pk.table,
                "column": pk.column,
                "key_ordinal": pk.key_ordinal,
            })).collect::<Vec<_>>(),
            "foreign_keys": foreign_keys.iter().map(|fk| json!({
                "constraint_name": fk.constraint_name,
                "from_schema": fk.from_schema,
                "from_table": fk.from_table,
                "from_column": fk.from_column,
                "to_schema": fk.to_schema,
                "to_table": fk.to_table,
                "to_column": fk.to_column,
            })).collect::<Vec<_>>(),
            "indexes": indexes.iter().map(metadata_node_json).collect::<Vec<_>>(),
        });

        let rendered = if args.format == OutputFormat::Json {
            serde_json::to_string_pretty(&payload).expect("describe json")
        } else {
            render_describe_sections(
                args.format,
                &columns,
                &primary_keys,
                &foreign_keys,
                &indexes,
            )
        };
        Ok::<_, CliError>((rendered, payload))
    }
    .await;

    match result {
        Ok((text, data)) => crate::agent::emit(json, Some(text), Ok(data)),
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

pub fn split_object(object: &str) -> Result<(String, String), CliError> {
    if let Some((schema, name)) = object.split_once('.') {
        if schema.is_empty() || name.is_empty() {
            return Err(CliError::usage("object must be schema.name or name"));
        }
        return Ok((schema.to_string(), name.to_string()));
    }
    Ok(("dbo".to_string(), object.to_string()))
}

fn nodes_to_rows(nodes: Vec<MetadataNode>, kind: &str) -> Vec<(String, String, String)> {
    nodes
        .into_iter()
        .map(|node| object_row(&node, kind))
        .collect()
}

fn object_row(node: &MetadataNode, kind: &str) -> (String, String, String) {
    (
        node.schema.clone().unwrap_or_default(),
        node.name.clone(),
        kind.to_string(),
    )
}

fn programmable_kind(kind: &ObjectKind) -> &'static str {
    match kind {
        ObjectKind::StoredProcedure => "procedure",
        ObjectKind::ScalarFunction | ObjectKind::TableFunction | ObjectKind::AggregateFunction => {
            "function"
        }
        _ => "object",
    }
}

fn rows_to_json(rows: &[(String, String, String)]) -> Vec<Value> {
    rows.iter()
        .map(|(schema, name, kind)| json!({ "schema": schema, "name": name, "kind": kind }))
        .collect()
}

fn render_object_rows(rows: &[(String, String, String)], format: OutputFormat) -> String {
    let set = ResultSet {
        columns: vec![
            sqlly_core::ResultColumn {
                name: "schema".into(),
                data_type: "text".into(),
                nullable: false,
            },
            sqlly_core::ResultColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: false,
            },
            sqlly_core::ResultColumn {
                name: "kind".into(),
                data_type: "text".into(),
                nullable: false,
            },
        ],
        rows: rows
            .iter()
            .map(|(schema, name, kind)| {
                vec![
                    sqlly_core::CellValue::String(schema.clone()),
                    sqlly_core::CellValue::String(name.clone()),
                    sqlly_core::CellValue::String(kind.clone()),
                ]
            })
            .collect(),
        rows_affected: None,
        truncated: false,
    };
    format::render(format, &[set])
}

fn render_describe_sections(
    format: OutputFormat,
    columns: &[MetadataNode],
    primary_keys: &[sqlly_services::metadata::PrimaryKeyRow],
    foreign_keys: &[sqlly_services::metadata::ForeignKeyRow],
    indexes: &[MetadataNode],
) -> String {
    let mut out = String::new();
    out.push_str("## Columns\n");
    out.push_str(&render_metadata_nodes(format, columns));
    out.push_str("\n## Primary key\n");
    out.push_str(&render_pk_rows(format, primary_keys));
    out.push_str("\n## Foreign keys\n");
    out.push_str(&render_fk_rows(format, foreign_keys));
    out.push_str("\n## Indexes\n");
    out.push_str(&render_metadata_nodes(format, indexes));
    out
}

fn render_metadata_nodes(format: OutputFormat, nodes: &[MetadataNode]) -> String {
    let rows: Vec<_> = nodes
        .iter()
        .map(|n| {
            vec![
                sqlly_core::CellValue::String(n.name.clone()),
                sqlly_core::CellValue::String(n.detail.clone().unwrap_or_default()),
            ]
        })
        .collect();
    let set = ResultSet {
        columns: vec![
            sqlly_core::ResultColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: false,
            },
            sqlly_core::ResultColumn {
                name: "detail".into(),
                data_type: "text".into(),
                nullable: true,
            },
        ],
        rows,
        rows_affected: None,
        truncated: false,
    };
    format::render(format, &[set])
}

fn render_pk_rows(
    format: OutputFormat,
    rows: &[sqlly_services::metadata::PrimaryKeyRow],
) -> String {
    let set = ResultSet {
        columns: vec![sqlly_core::ResultColumn {
            name: "column".into(),
            data_type: "text".into(),
            nullable: false,
        }],
        rows: rows
            .iter()
            .map(|pk| vec![sqlly_core::CellValue::String(pk.column.clone())])
            .collect(),
        rows_affected: None,
        truncated: false,
    };
    format::render(format, &[set])
}

fn render_fk_rows(
    format: OutputFormat,
    rows: &[sqlly_services::metadata::ForeignKeyRow],
) -> String {
    let set = ResultSet {
        columns: vec![
            sqlly_core::ResultColumn {
                name: "constraint".into(),
                data_type: "text".into(),
                nullable: false,
            },
            sqlly_core::ResultColumn {
                name: "from".into(),
                data_type: "text".into(),
                nullable: false,
            },
            sqlly_core::ResultColumn {
                name: "to".into(),
                data_type: "text".into(),
                nullable: false,
            },
        ],
        rows: rows
            .iter()
            .map(|fk| {
                vec![
                    sqlly_core::CellValue::String(fk.constraint_name.clone()),
                    sqlly_core::CellValue::String(format!(
                        "{}.{}.{}",
                        fk.from_schema, fk.from_table, fk.from_column
                    )),
                    sqlly_core::CellValue::String(format!(
                        "{}.{}.{}",
                        fk.to_schema, fk.to_table, fk.to_column
                    )),
                ]
            })
            .collect(),
        rows_affected: None,
        truncated: false,
    };
    format::render(format, &[set])
}

fn metadata_node_json(node: &MetadataNode) -> Value {
    json!({
        "name": node.name,
        "schema": node.schema,
        "kind": node.kind,
        "detail": node.detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_describe_splits_schema_and_name() {
        assert_eq!(
            split_object("dbo.Orders").unwrap(),
            ("dbo".to_string(), "Orders".to_string())
        );
        assert_eq!(
            split_object("Orders").unwrap(),
            ("dbo".to_string(), "Orders".to_string())
        );
    }
}
