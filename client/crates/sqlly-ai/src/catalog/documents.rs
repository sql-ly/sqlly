//! Stable search-document generation for catalog objects.

use sha2::{Digest, Sha256};

use crate::catalog::wire::CatalogObject;

pub const DOCUMENT_GENERATOR_VERSION: i64 = 1;
const MAX_DEFINITION_CHARS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchDocument {
    pub object_key: String,
    pub document_type: String,
    pub title: String,
    pub body: String,
    pub keywords: String,
    pub content_hash: String,
    pub generator_version: i64,
}

pub fn generate_document(object_key: &str, obj: &CatalogObject) -> SearchDocument {
    let title = match &obj.schema {
        Some(schema) if !schema.is_empty() => format!("{schema}.{}", obj.name),
        _ => obj.name.clone(),
    };

    let mut keywords = Vec::new();
    push_identifier_terms(&mut keywords, &obj.name);
    if let Some(schema) = &obj.schema {
        push_identifier_terms(&mut keywords, schema);
    }
    keywords.push(obj.kind.as_str().to_string());

    let mut body = vec![format!("{} {title}", obj.kind.as_str())];

    for column in &obj.columns {
        push_identifier_terms(&mut keywords, &column.name);
        body.push(format!(
            "column {} {}{}",
            column.name,
            column.type_name.as_deref().unwrap_or(""),
            if column.nullable {
                " nullable"
            } else {
                " not null"
            }
        ));
    }

    for parameter in &obj.parameters {
        push_identifier_terms(&mut keywords, &parameter.name);
        body.push(format!(
            "parameter {} {}{}",
            parameter.name,
            parameter.type_name.as_deref().unwrap_or(""),
            if parameter.is_output { " output" } else { "" }
        ));
    }

    for fk in &obj.foreign_keys {
        for column in &fk.columns {
            push_identifier_terms(&mut keywords, column);
        }
        push_identifier_terms(&mut keywords, &fk.target_table);
        for column in &fk.target_columns {
            push_identifier_terms(&mut keywords, column);
        }
        let target = match &fk.target_schema {
            Some(schema) if !schema.is_empty() => format!("{schema}.{}", fk.target_table),
            _ => fk.target_table.clone(),
        };
        body.push(format!(
            "foreign key {} references {} ({})",
            fk.columns.join(", "),
            target,
            fk.target_columns.join(", ")
        ));
    }

    if let Some(definition) = &obj.definition {
        body.push(truncate_chars(definition, MAX_DEFINITION_CHARS));
    }

    let body = body.join("\n");
    let keywords = keywords.join(" ");
    let content_hash = hash_parts(&title, &body, &keywords);

    SearchDocument {
        object_key: object_key.to_string(),
        document_type: "object_card".to_string(),
        title,
        body,
        keywords,
        content_hash,
        generator_version: DOCUMENT_GENERATOR_VERSION,
    }
}

fn push_identifier_terms(out: &mut Vec<String>, value: &str) {
    let mut current = String::new();
    let mut prev_lower = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if prev_lower && ch.is_ascii_uppercase() && !current.is_empty() {
                out.push(current.to_ascii_lowercase());
                current.clear();
            }
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
            current.push(ch);
        } else {
            if !current.is_empty() {
                out.push(current.to_ascii_lowercase());
                current.clear();
            }
            prev_lower = false;
        }
    }
    if !current.is_empty() {
        out.push(current.to_ascii_lowercase());
    }
    let folded = value.to_ascii_lowercase();
    if folded.chars().any(|c| c.is_ascii_alphanumeric()) {
        out.push(folded);
    }
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    value.chars().take(max).collect()
}

fn hash_parts(title: &str, body: &str, keywords: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    hasher.update(b"\0");
    hasher.update(body.as_bytes());
    hasher.update(b"\0");
    hasher.update(keywords.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::wire::{
        CatalogColumn, CatalogForeignKey, CatalogObjectKind, CatalogSource, SourceKind,
    };

    fn object() -> CatalogObject {
        CatalogObject {
            schema: Some("dbo".into()),
            name: "JournalLine".into(),
            kind: CatalogObjectKind::Table,
            columns: vec![CatalogColumn {
                name: "tenant_id".into(),
                type_name: Some("INT".into()),
                ordinal: 1,
                nullable: false,
            }],
            primary_key: vec![],
            foreign_keys: vec![CatalogForeignKey {
                columns: vec!["tenant_id".into()],
                target_schema: Some("dbo".into()),
                target_table: "Tenant".into(),
                target_columns: vec!["id".into()],
            }],
            parameters: vec![],
            definition: Some("CREATE TABLE dbo.JournalLine (tenant_id INT)".into()),
            source: CatalogSource {
                kind: SourceKind::Database,
                path: None,
                create_date: Some(1),
                modify_date: Some(2),
            },
        }
    }

    #[test]
    fn generates_object_document_with_columns_and_fk_targets() {
        let doc = generate_document("dbo.journalline", &object());
        assert_eq!(doc.title, "dbo.JournalLine");
        assert!(doc.body.contains("column tenant_id INT"));
        assert!(doc.body.contains("references dbo.Tenant"));
        assert!(doc.keywords.contains("journal"));
        assert!(doc.keywords.contains("tenant_id"));
        assert_eq!(doc.generator_version, DOCUMENT_GENERATOR_VERSION);
    }

    #[test]
    fn document_hash_ignores_source_timestamps() {
        let first = object();
        let mut second = object();
        second.source.modify_date = Some(99);
        assert_eq!(
            generate_document("dbo.journalline", &first).content_hash,
            generate_document("dbo.journalline", &second).content_hash
        );
    }
}
