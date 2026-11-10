//! Object cards + token-budgeted context assembly.
//!
//! A "card" is a compact, LLM-readable description of one catalog object. We
//! never ship the whole schema to the model; instead `assemble_context` picks a
//! small set of objects relevant to a question and renders their compact cards
//! until a token budget is exhausted.
//!
//! When an object's AI cache is *valid*, we prefer its prebuilt (richer) card;
//! otherwise we synthesize a compact card on the fly from the raw catalog body.
//! This is what makes `ask` work even when the AI-cache feature is off.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::catalog::prompt::{fence_schema_data, wrap_llm_schema_context};
use crate::catalog::store::{CatalogStore, StoredObject, StoredObjectCard};
use crate::catalog::wire::CatalogObject;

pub const CARD_FORMAT_VERSION: i64 = 1;

/// Approximate tokens-per-character used for budget accounting. Rough but
/// stable; we err toward over-counting so we never blow the model's context.
const CHARS_PER_TOKEN: f64 = 4.0;

/// Estimate the token cost of a string.
pub fn estimate_tokens(s: &str) -> usize {
    ((s.chars().count() as f64) / CHARS_PER_TOKEN).ceil() as usize
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardType {
    Tiny,
    Compact,
    SqlGeneration,
    Safety,
    BusinessMeaning,
}

impl CardType {
    pub fn as_str(self) -> &'static str {
        match self {
            CardType::Tiny => "tiny",
            CardType::Compact => "compact",
            CardType::SqlGeneration => "sql_generation",
            CardType::Safety => "safety",
            CardType::BusinessMeaning => "business_meaning",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "tiny" => Some(Self::Tiny),
            "compact" => Some(Self::Compact),
            "sql_generation" => Some(Self::SqlGeneration),
            "safety" => Some(Self::Safety),
            "business_meaning" => Some(Self::BusinessMeaning),
            _ => None,
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SchemaPurpose {
    #[default]
    General,
    SqlGeneration,
    SqlReview,
    Explanation,
    SafetyCheck,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedObjectCard {
    pub card_type: CardType,
    pub content: String,
    pub token_estimate: usize,
    pub content_hash: String,
    pub generated_by: String,
    pub card_format_version: i64,
    pub confidence: f64,
    pub contains_model_inference: bool,
}

/// A compact card describing one object.
#[derive(Debug, Clone)]
pub struct ObjectCard {
    pub object_key: String,
    pub tokens: usize,
    pub text: String,
}

pub fn tiny_card_text(obj: &CatalogObject) -> String {
    let qual = qualified_name(obj);
    let columns = obj
        .columns
        .iter()
        .take(4)
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if columns.is_empty() {
        format!("{} {qual}", obj.kind.as_str().to_ascii_uppercase())
    } else {
        format!(
            "{} {qual}: {columns}",
            obj.kind.as_str().to_ascii_uppercase()
        )
    }
}

pub fn compact_card_text(obj: &CatalogObject) -> String {
    let kind = obj.kind.as_str().to_uppercase();
    let qual = qualified_name(obj);

    let mut lines = vec![format!("{kind} {qual}")];

    match obj.kind {
        crate::catalog::wire::CatalogObjectKind::Table
        | crate::catalog::wire::CatalogObjectKind::View => {
            if obj.columns.is_empty() {
                lines.push("  (no columns observed)".into());
            } else {
                let cols: Vec<String> = obj
                    .columns
                    .iter()
                    .map(|c| match &c.type_name {
                        Some(t) => format!("{} {}", c.name, t),
                        None => c.name.clone(),
                    })
                    .collect();
                lines.push(format!("  columns: {}", cols.join(", ")));
            }
            for fk in &obj.foreign_keys {
                let from = fk.columns.join(", ");
                let to_cols = fk.target_columns.join(", ");
                let target_qual = match &fk.target_schema {
                    Some(s) => format!("{}.{}", s, fk.target_table),
                    None => fk.target_table.clone(),
                };
                lines.push(format!("  FK: {from} -> {target_qual}({to_cols})"));
            }
        }
        crate::catalog::wire::CatalogObjectKind::StoredProcedure
        | crate::catalog::wire::CatalogObjectKind::Function => {
            let params: Vec<String> = obj
                .parameters
                .iter()
                .map(|p| match (&p.type_name, p.is_output) {
                    (Some(t), true) => format!("{} {} OUT", p.name, t),
                    (Some(t), false) => format!("{} {}", p.name, t),
                    (None, true) => format!("{} OUT", p.name),
                    (None, false) => p.name.clone(),
                })
                .collect();
            if !params.is_empty() {
                lines.push(format!("  params: {}", params.join(", ")));
            }
        }
        crate::catalog::wire::CatalogObjectKind::Type => {
            if !obj.columns.is_empty() {
                let cols: Vec<String> = obj.columns.iter().map(|c| c.name.clone()).collect();
                lines.push(format!("  attributes: {}", cols.join(", ")));
            }
        }
    }

    if let Some(def) = &obj.definition {
        let snippet = truncate(def, 200);
        lines.push(format!("  definition: {}", fence_schema_data(&snippet)));
    }

    lines.join("\n")
}

pub fn sql_generation_card_text(obj: &CatalogObject) -> String {
    let mut lines = vec![format!(
        "{} {}",
        obj.kind.as_str().to_ascii_uppercase(),
        qualified_name(obj)
    )];
    if !obj.columns.is_empty() {
        lines.push("  columns:".into());
        for col in &obj.columns {
            let typ = col.type_name.as_deref().unwrap_or("UNKNOWN");
            let nullable = if col.nullable { "NULL" } else { "NOT NULL" };
            lines.push(format!("    - {} {} {nullable}", col.name, typ));
        }
    }
    if !obj.parameters.is_empty() {
        lines.push("  parameters:".into());
        for param in &obj.parameters {
            let typ = param.type_name.as_deref().unwrap_or("UNKNOWN");
            let direction = if param.is_output { " OUT" } else { "" };
            lines.push(format!("    - {} {}{direction}", param.name, typ));
        }
    }
    if !obj.foreign_keys.is_empty() {
        lines.push("  known foreign keys:".into());
        for fk in &obj.foreign_keys {
            let target_qual = match &fk.target_schema {
                Some(schema) => format!("{}.{}", schema, fk.target_table),
                None => fk.target_table.clone(),
            };
            lines.push(format!(
                "    - {}({}) -> {}({})",
                qualified_name(obj),
                fk.columns.join(", "),
                target_qual,
                fk.target_columns.join(", ")
            ));
        }
    }
    lines.join("\n")
}

pub fn safety_card_text(obj: &CatalogObject) -> Option<String> {
    let mut indicators = Vec::new();
    let object_name = obj.name.to_ascii_lowercase();
    for risky in [
        "ledger",
        "journal",
        "payment",
        "invoice",
        "accounting",
        "audit",
    ] {
        if object_name.contains(risky) {
            indicators.push(format!("object name suggests sensitive {risky} data"));
        }
    }
    for col in &obj.columns {
        let name = col.name.to_ascii_lowercase().replace('_', "");
        if matches!(
            name.as_str(),
            "tenantid" | "accountid" | "organizationid" | "orgid"
        ) {
            indicators.push(format!(
                "column {} may be a tenant/account boundary",
                col.name
            ));
        }
        if name.contains("password") || name.contains("secret") || name.contains("token") {
            indicators.push(format!("column {} appears credential-like", col.name));
        }
    }
    if indicators.is_empty() {
        return None;
    }
    let mut lines = vec![format!("SAFETY {}", qualified_name(obj))];
    for indicator in indicators {
        lines.push(format!("  - Possible indicator: {indicator}"));
    }
    Some(lines.join("\n"))
}

pub fn generate_metadata_cards(obj: &CatalogObject) -> Vec<GeneratedObjectCard> {
    let mut cards = vec![
        metadata_card(CardType::Tiny, tiny_card_text(obj)),
        metadata_card(CardType::Compact, compact_card_text(obj)),
        metadata_card(CardType::SqlGeneration, sql_generation_card_text(obj)),
    ];
    if let Some(safety) = safety_card_text(obj) {
        cards.push(metadata_card(CardType::Safety, safety));
    }
    cards
}

pub fn business_meaning_card(content: String, generated_by: &str) -> GeneratedObjectCard {
    GeneratedObjectCard {
        card_type: CardType::BusinessMeaning,
        token_estimate: estimate_tokens(&content),
        content_hash: card_content_hash(CardType::BusinessMeaning, &content),
        content,
        generated_by: generated_by.to_string(),
        card_format_version: CARD_FORMAT_VERSION,
        confidence: 0.7,
        contains_model_inference: true,
    }
}

fn metadata_card(card_type: CardType, content: String) -> GeneratedObjectCard {
    GeneratedObjectCard {
        card_type,
        token_estimate: estimate_tokens(&content),
        content_hash: card_content_hash(card_type, &content),
        content,
        generated_by: "deterministic_metadata".into(),
        card_format_version: CARD_FORMAT_VERSION,
        confidence: 1.0,
        contains_model_inference: false,
    }
}

pub fn card_content_hash(card_type: CardType, content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CARD_FORMAT_VERSION.to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(card_type.as_str().as_bytes());
    hasher.update(b"\n");
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn qualified_name(obj: &CatalogObject) -> String {
    match &obj.schema {
        Some(ns) if !ns.is_empty() => format!("{ns}.{}", obj.name),
        _ => obj.name.clone(),
    }
}

/// Render a compact card for an object from its stored body.
///
/// Format (table example):
/// ```text
/// TABLE dbo.users (id INT, email VARCHAR(255), ...)
///   PK: id
///   FK: user_id -> dbo.users(id)
/// ```
pub fn compact_card(obj: &StoredObject) -> ObjectCard {
    let parsed: Option<CatalogObject> = serde_json::from_str(&obj.body_json).ok();
    let text = match parsed {
        Some(parsed) => compact_card_text(&parsed),
        None => {
            let kind = obj.kind.as_str().to_uppercase();
            let qual = match &obj.namespace {
                Some(ns) if !ns.is_empty() => format!("{ns}.{}", obj.name),
                _ => obj.name.clone(),
            };
            format!("{kind} {qual}")
        }
    };
    let tokens = estimate_tokens(&text);
    ObjectCard {
        object_key: obj.object_key.clone(),
        tokens,
        text,
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// The result of assembling context for a question.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AssembledContext {
    pub text: String,
    pub tokens: usize,
    pub object_keys: Vec<String>,
    pub used_prebuilt_cards: usize,
    pub purpose: SchemaPurpose,
}

/// Assemble a compact context block from the given objects, stopping when the
/// token budget is reached.
///
/// Objects should already be ordered by relevance (most relevant first). Each
/// object contributes either its prebuilt AI card (when valid) or a freshly
/// synthesized compact card.
pub fn assemble_context(objects: &[StoredObject], max_tokens: usize) -> AssembledContext {
    assemble_context_for_purpose(objects, max_tokens, SchemaPurpose::General)
}

pub fn assemble_context_for_purpose(
    objects: &[StoredObject],
    max_tokens: usize,
    purpose: SchemaPurpose,
) -> AssembledContext {
    let mut cards: Vec<ObjectCard> = Vec::new();
    let mut total = 0usize;
    let mut used_prebuilt = 0usize;

    for obj in objects {
        let card = match &obj.ai_card_json {
            Some(json) if obj.ai_cache_valid => {
                if let Some(text) = extract_prebuilt_text(json) {
                    let tokens = estimate_tokens(&text);
                    used_prebuilt += 1;
                    ObjectCard {
                        object_key: obj.object_key.clone(),
                        tokens,
                        text,
                    }
                } else {
                    compact_card(obj)
                }
            }
            _ => compact_card(obj),
        };
        if total + card.tokens > max_tokens && !cards.is_empty() {
            break;
        }
        total += card.tokens;
        cards.push(card);
    }

    let text = wrap_llm_schema_context(
        &cards
            .iter()
            .map(|c| c.text.clone())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );

    AssembledContext {
        tokens: total,
        text,
        object_keys: cards.into_iter().map(|c| c.object_key).collect(),
        used_prebuilt_cards: used_prebuilt,
        purpose,
    }
}

pub fn assemble_context_from_store(
    store: &CatalogStore,
    objects: &[StoredObject],
    max_tokens: usize,
    purpose: SchemaPurpose,
) -> crate::catalog::store::CatalogResult<AssembledContext> {
    let keys: Vec<String> = objects.iter().map(|o| o.object_key.clone()).collect();
    let card_types = card_types_for_purpose(purpose);
    let card_cache = store.cards_for_keys_and_types(&keys, &card_types)?;

    let mut cards: Vec<ObjectCard> = Vec::new();
    let mut total = 0usize;
    let mut used_prebuilt = 0usize;

    for obj in objects {
        let mut object_cards = cards_for_purpose_cached(store, obj, purpose, &card_cache)?;
        if object_cards.is_empty() {
            object_cards.push(compact_card(obj));
        }
        for card in object_cards {
            if total + card.tokens > max_tokens && !cards.is_empty() {
                return Ok(finish_context(cards, total, used_prebuilt, purpose));
            }
            if matches!(purpose, SchemaPurpose::General | SchemaPurpose::Explanation)
                && card.text != compact_card(obj).text
            {
                used_prebuilt += 1;
            }
            total += card.tokens;
            cards.push(card);
        }
    }

    Ok(finish_context(cards, total, used_prebuilt, purpose))
}

fn card_types_for_purpose(purpose: SchemaPurpose) -> Vec<CardType> {
    match purpose {
        SchemaPurpose::General | SchemaPurpose::Explanation => {
            vec![CardType::BusinessMeaning, CardType::Compact]
        }
        SchemaPurpose::SqlGeneration | SchemaPurpose::SqlReview => {
            vec![CardType::Safety, CardType::SqlGeneration]
        }
        SchemaPurpose::SafetyCheck => vec![CardType::Safety, CardType::Compact],
    }
}

fn finish_context(
    cards: Vec<ObjectCard>,
    total: usize,
    used_prebuilt: usize,
    purpose: SchemaPurpose,
) -> AssembledContext {
    let text = wrap_llm_schema_context(
        &cards
            .iter()
            .map(|c| c.text.clone())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );
    AssembledContext {
        tokens: total,
        text,
        object_keys: cards.into_iter().map(|c| c.object_key).collect(),
        used_prebuilt_cards: used_prebuilt,
        purpose,
    }
}

fn cards_for_purpose_cached(
    store: &CatalogStore,
    obj: &StoredObject,
    purpose: SchemaPurpose,
    cache: &HashMap<(String, CardType), StoredObjectCard>,
) -> crate::catalog::store::CatalogResult<Vec<ObjectCard>> {
    if matches!(purpose, SchemaPurpose::General | SchemaPurpose::Explanation) {
        if obj.ai_cache_valid
            && let Some(card) =
                stored_or_rendered_card_cached(store, obj, CardType::BusinessMeaning, cache)?
        {
            return Ok(vec![card]);
        }
        return Ok(
            stored_or_rendered_card_cached(store, obj, CardType::Compact, cache)?
                .into_iter()
                .collect(),
        );
    }

    let types: &[CardType] = match purpose {
        SchemaPurpose::SqlGeneration | SchemaPurpose::SqlReview => {
            &[CardType::Safety, CardType::SqlGeneration]
        }
        SchemaPurpose::SafetyCheck => &[CardType::Safety, CardType::Compact],
        SchemaPurpose::General | SchemaPurpose::Explanation => unreachable!(),
    };
    let mut out = Vec::new();
    for card_type in types {
        if let Some(card) = stored_or_rendered_card_cached(store, obj, *card_type, cache)? {
            out.push(card);
        }
    }
    Ok(out)
}

fn stored_or_rendered_card_cached(
    store: &CatalogStore,
    obj: &StoredObject,
    card_type: CardType,
    cache: &HashMap<(String, CardType), StoredObjectCard>,
) -> crate::catalog::store::CatalogResult<Option<ObjectCard>> {
    if let Some(stored) = cache.get(&(obj.object_key.clone(), card_type))
        && stored.card_format_version == CARD_FORMAT_VERSION
    {
        return Ok(Some(object_card_from_stored(stored.clone())));
    }
    stored_or_rendered_card(store, obj, card_type)
}

fn stored_or_rendered_card(
    store: &CatalogStore,
    obj: &StoredObject,
    card_type: CardType,
) -> crate::catalog::store::CatalogResult<Option<ObjectCard>> {
    if let Some(stored) = store.card_for_object_type(&obj.object_key, card_type)?
        && stored.card_format_version == CARD_FORMAT_VERSION
    {
        return Ok(Some(object_card_from_stored(stored)));
    }
    Ok(render_card_on_the_fly(obj, card_type))
}

fn object_card_from_stored(card: StoredObjectCard) -> ObjectCard {
    ObjectCard {
        object_key: card.object_key,
        tokens: card.token_estimate,
        text: card.content,
    }
}

fn render_card_on_the_fly(obj: &StoredObject, card_type: CardType) -> Option<ObjectCard> {
    if card_type == CardType::BusinessMeaning {
        return match &obj.ai_card_json {
            Some(json) if obj.ai_cache_valid => {
                extract_prebuilt_text(json).map(|text| ObjectCard {
                    object_key: obj.object_key.clone(),
                    tokens: estimate_tokens(&text),
                    text,
                })
            }
            _ => None,
        };
    }
    let parsed = obj.to_catalog_object().ok()?;
    let text = match card_type {
        CardType::Tiny => tiny_card_text(&parsed),
        CardType::Compact => compact_card_text(&parsed),
        CardType::SqlGeneration => sql_generation_card_text(&parsed),
        CardType::Safety => safety_card_text(&parsed)?,
        CardType::BusinessMeaning => unreachable!(),
    };
    Some(ObjectCard {
        object_key: obj.object_key.clone(),
        tokens: estimate_tokens(&text),
        text,
    })
}

/// Pull a human-readable text blob out of a prebuilt card JSON. Accepts either
/// a `{"text": "..."}` shape or a raw string.
pub(crate) fn extract_prebuilt_text(json: &str) -> Option<String> {
    let trimmed = json.trim();
    if trimmed.starts_with('{') {
        #[derive(serde::Deserialize)]
        struct CardShape {
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            summary: Option<String>,
        }
        let shape: CardShape = serde_json::from_str(json).ok()?;
        shape.text.or(shape.summary)
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::store::CatalogStore;
    use crate::catalog::wire::{
        CatalogColumn, CatalogForeignKey, CatalogObject, CatalogObjectKind, CatalogSource,
        SourceKind,
    };

    fn db_source() -> CatalogSource {
        CatalogSource {
            kind: SourceKind::Database,
            path: None,
            create_date: Some(1),
            modify_date: Some(2),
        }
    }

    fn stored(store: &CatalogStore, key: &str) -> StoredObject {
        store.get_object(key).unwrap().unwrap()
    }

    fn sync_table(store: &CatalogStore) {
        let mut users = CatalogObject {
            schema: Some("dbo".into()),
            name: "users".into(),
            kind: CatalogObjectKind::Table,
            columns: vec![
                CatalogColumn {
                    name: "id".into(),
                    type_name: Some("INT".into()),
                    ordinal: 1,
                    nullable: false,
                },
                CatalogColumn {
                    name: "email".into(),
                    type_name: Some("VARCHAR(255)".into()),
                    ordinal: 2,
                    nullable: false,
                },
            ],
            primary_key: vec![],
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source: db_source(),
        };
        let mut orders = CatalogObject {
            schema: Some("dbo".into()),
            name: "orders".into(),
            kind: CatalogObjectKind::Table,
            columns: vec![
                CatalogColumn {
                    name: "id".into(),
                    type_name: Some("INT".into()),
                    ordinal: 1,
                    nullable: false,
                },
                CatalogColumn {
                    name: "user_id".into(),
                    type_name: Some("INT".into()),
                    ordinal: 2,
                    nullable: false,
                },
            ],
            primary_key: vec![],
            foreign_keys: vec![CatalogForeignKey {
                columns: vec!["user_id".into()],
                target_schema: Some("dbo".into()),
                target_table: "users".into(),
                target_columns: vec!["id".into()],
            }],
            parameters: vec![],
            definition: None,
            source: db_source(),
        };
        let _ = &mut users;
        let _ = &mut orders;
        store.sync(&[users, orders]).unwrap();
    }

    #[test]
    fn compact_card_renders_table_with_columns_and_fk() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_table(&store);
        let card = compact_card(&stored(&store, "dbo.orders"));
        assert!(card.text.contains("TABLE dbo.orders"));
        assert!(card.text.contains("columns: id INT, user_id INT"));
        assert!(card.text.contains("FK: user_id -> dbo.users(id)"));
    }

    #[test]
    fn metadata_cards_include_sql_generation_and_safety_when_meaningful() {
        let obj = CatalogObject {
            schema: Some("dbo".into()),
            name: "payments".into(),
            kind: CatalogObjectKind::Table,
            columns: vec![
                CatalogColumn {
                    name: "tenant_id".into(),
                    type_name: Some("INT".into()),
                    ordinal: 1,
                    nullable: false,
                },
                CatalogColumn {
                    name: "amount".into(),
                    type_name: Some("DECIMAL(10,2)".into()),
                    ordinal: 2,
                    nullable: false,
                },
            ],
            primary_key: vec![],
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source: db_source(),
        };

        let cards = generate_metadata_cards(&obj);

        assert!(cards.iter().any(|card| card.card_type == CardType::Tiny));
        assert!(cards.iter().any(|card| card.card_type == CardType::Compact));
        assert!(
            cards
                .iter()
                .any(|card| card.card_type == CardType::SqlGeneration)
        );
        let safety = cards
            .iter()
            .find(|card| card.card_type == CardType::Safety)
            .expect("tenant/payment indicators should produce a safety card");
        assert!(safety.content.contains("tenant_id"));
        assert!(!safety.contains_model_inference);
    }

    #[test]
    fn card_content_hash_changes_with_type_and_version() {
        let tiny_hash = card_content_hash(CardType::Tiny, "same content");
        let compact_hash = card_content_hash(CardType::Compact, "same content");
        let versioned_content_hash = card_content_hash(
            CardType::Tiny,
            &format!("same content\nformat={CARD_FORMAT_VERSION}"),
        );

        assert_ne!(tiny_hash, compact_hash);
        assert_ne!(tiny_hash, versioned_content_hash);
    }

    #[test]
    fn compact_card_renders_routine_params() {
        let store = CatalogStore::open_in_memory().unwrap();
        let proc = CatalogObject {
            schema: Some("dbo".into()),
            name: "get_user".into(),
            kind: CatalogObjectKind::StoredProcedure,
            columns: vec![],
            primary_key: vec![],
            foreign_keys: vec![],
            parameters: vec![
                crate::catalog::wire::CatalogParameter {
                    name: "@id".into(),
                    type_name: Some("INT".into()),
                    is_output: false,
                },
                crate::catalog::wire::CatalogParameter {
                    name: "@rc".into(),
                    type_name: Some("INT".into()),
                    is_output: true,
                },
            ],
            definition: Some("SELECT * FROM users WHERE id = @id".into()),
            source: db_source(),
        };
        store.sync(&[proc]).unwrap();
        let card = compact_card(&stored(&store, "dbo.get_user"));
        assert!(card.text.contains("@id INT"));
        assert!(card.text.contains("@rc INT OUT"));
        assert!(card.text.contains("definition:"));
    }

    #[test]
    fn assemble_context_respects_token_budget() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_table(&store);
        let objs = store.live_objects().unwrap();
        // Tiny budget — should fit at most one object.
        let ctx = assemble_context(&objs, 15);
        assert!(ctx.object_keys.len() <= 2);
        assert!(ctx.tokens <= 60); // generous upper bound
    }

    #[test]
    fn assemble_context_prefers_prebuilt_card_when_valid() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_table(&store);
        store
            .set_ai_card("dbo.users", r#"{"text":"PREBUILT SUMMARY OF USERS"}"#)
            .unwrap();
        let objs = store.live_objects().unwrap();
        let users_first: Vec<StoredObject> = objs
            .iter()
            .filter(|o| o.object_key == "dbo.users")
            .cloned()
            .collect();
        let ctx = assemble_context(&users_first, 10_000);
        assert_eq!(ctx.used_prebuilt_cards, 1);
        assert!(ctx.text.contains("PREBUILT SUMMARY"));
    }

    #[test]
    fn assemble_context_falls_back_when_ai_cache_invalid() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_table(&store);
        // Set a prebuilt card, then mutate the object → cache invalidated.
        store
            .set_ai_card("dbo.users", r#"{"text":"PREBUILT"}"#)
            .unwrap();
        let changed = stored(&store, "dbo.users");
        // force a change by re-syncing with an extra column
        let mut obj = changed.to_catalog_object().unwrap();
        obj.columns.push(CatalogColumn {
            name: "name".into(),
            type_name: Some("VARCHAR(50)".into()),
            ordinal: 3,
            nullable: true,
        });
        obj.source.modify_date = Some(3);
        store.sync(&[obj]).unwrap();
        let objs = store.live_objects().unwrap();
        let users: Vec<StoredObject> = objs
            .into_iter()
            .filter(|o| o.object_key == "dbo.users")
            .collect();
        let ctx = assemble_context(&users, 10_000);
        // Cache invalid → must synthesize on the fly, no prebuilt used.
        assert_eq!(ctx.used_prebuilt_cards, 0);
        assert!(ctx.text.contains("TABLE dbo.users"));
    }

    #[test]
    fn assemble_context_from_store_uses_purpose_specific_cards() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_table(&store);
        let objs = store.objects_by_keys(&["dbo.orders".to_string()]).unwrap();

        let ctx = assemble_context_from_store(&store, &objs, 10_000, SchemaPurpose::SqlGeneration)
            .unwrap();

        assert_eq!(ctx.purpose, SchemaPurpose::SqlGeneration);
        assert!(ctx.text.contains("known foreign keys"));
        assert!(ctx.text.contains("dbo.orders(user_id) -> dbo.users(id)"));
    }

    #[test]
    fn assemble_context_from_store_ignores_stale_business_meaning() {
        let store = CatalogStore::open_in_memory().unwrap();
        sync_table(&store);
        store
            .set_ai_card("dbo.users", r#"{"text":"STALE BUSINESS MEANING"}"#)
            .unwrap();

        let mut users = store.get_object("dbo.users").unwrap().unwrap();
        let mut obj = users.to_catalog_object().unwrap();
        obj.columns.push(CatalogColumn {
            name: "display_name".into(),
            type_name: Some("VARCHAR(80)".into()),
            ordinal: 3,
            nullable: true,
        });
        obj.source.modify_date = Some(4);
        store.sync(&[obj]).unwrap();

        users = store.get_object("dbo.users").unwrap().unwrap();
        assert!(!users.ai_cache_valid);
        let ctx =
            assemble_context_from_store(&store, &[users], 10_000, SchemaPurpose::General).unwrap();

        assert_eq!(ctx.used_prebuilt_cards, 0);
        assert!(!ctx.text.contains("STALE BUSINESS MEANING"));
        assert!(ctx.text.contains("display_name"));
    }

    #[test]
    fn estimate_tokens_is_monotonic() {
        assert!(estimate_tokens("a") <= estimate_tokens("aaaa"));
        assert!(estimate_tokens("hello world") > 0);
    }
}
