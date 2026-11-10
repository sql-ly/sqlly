//! Deterministic, parser-free inference of FK-like relationships from column
//! naming conventions, for catalogs whose source databases lack declared
//! foreign keys.
//!
//! This module is intentionally **pure**: it has no database access and operates
//! on plain Rust data ([`InferenceObject`] sources + a [`CandidateTargetIndex`]
//! of identity columns). The store builds the index once per sync and persists
//! the emitted [`InferredEdge`]s into `catalog_relationship` with
//! `is_inferred = 1`. Declared foreign keys always remain authoritative; inferred
//! edges are confidence-scored and conservatively filtered so a noisy guess never
//! outranks a real key.
//!
//! Design constraints:
//! - No new crates, no NLP/pluralization dependency — singularization is a tiny
//!   deterministic rule set.
//! - Conservative by default: ambiguous matches are skipped rather than emitted.
//! - Bounded work: candidate lookup is hash-map based (no O(objects²) scan), and
//!   only identity columns are indexed (not every column).

use std::collections::HashMap;

/// Tunable thresholds and scoring weights for inference. Defaults are
/// deliberately conservative.
#[derive(Debug, Clone, PartialEq)]
pub struct InferenceOptions {
    /// Minimum confidence for an edge to be emitted at all.
    pub min_confidence: f64,
    /// Bonus when source and target live in the same namespace/schema.
    pub same_namespace_bonus: f64,
    /// Bonus when source/target column types are compatible (both known).
    pub type_match_bonus: f64,
    /// Bonus when the target identity column is the specific
    /// `<entity>id`/`<entity>key`/`<entity>code` form rather than a generic `id`.
    pub exact_identity_bonus: f64,
    /// Penalty applied per extra candidate when more than one target shares the
    /// same normalized entity name (ambiguity).
    pub ambiguity_penalty: f64,
    /// Penalty for low-value audit columns (`created_by_id`, …).
    pub audit_column_penalty: f64,
}

impl Default for InferenceOptions {
    fn default() -> Self {
        Self {
            min_confidence: 0.55,
            same_namespace_bonus: 0.10,
            type_match_bonus: 0.10,
            exact_identity_bonus: 0.10,
            ambiguity_penalty: 0.10,
            audit_column_penalty: 0.20,
        }
    }
}

/// A column on a source object considered as a possible foreign key.
#[derive(Debug, Clone, PartialEq)]
pub struct InferenceColumn {
    pub name: String,
    pub type_name: Option<String>,
}

/// A live catalog object viewed as a candidate inference source and/or target.
#[derive(Debug, Clone, PartialEq)]
pub struct InferenceObject {
    pub object_key: String,
    pub namespace: Option<String>,
    pub name: String,
    /// Only table/view-like objects participate (routines/types do not).
    pub is_table_like: bool,
    pub columns: Vec<InferenceColumn>,
}

/// A resolved identity column on a potential target object.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateTarget {
    pub object_key: String,
    pub namespace: Option<String>,
    pub object_name: String,
    /// The actual identity column name (e.g. `ReportingPeriodId` or `Id`).
    pub identity_column: String,
    pub identity_type: Option<String>,
    /// `true` when `identity_column` is the specific `<entity>id` form, `false`
    /// when it is a generic `id`/`key`/`code`.
    pub identity_is_specific: bool,
}

/// Hash-map index of candidate targets keyed by normalized+singularized entity
/// name, so source-column lookup is O(1).
#[derive(Debug, Clone, Default)]
pub struct CandidateTargetIndex {
    by_entity: HashMap<String, Vec<CandidateTarget>>,
}

impl CandidateTargetIndex {
    /// Build the index from the live table-like objects. Each object contributes
    /// at most one identity column (chosen by priority).
    pub fn build(objects: &[InferenceObject]) -> Self {
        let mut by_entity: HashMap<String, Vec<CandidateTarget>> = HashMap::new();
        for obj in objects {
            if !obj.is_table_like {
                continue;
            }
            let Some((identity_column, identity_type, identity_is_specific)) =
                pick_identity_column(obj)
            else {
                continue;
            };
            let entity = entity_key_from_table(&obj.name);
            if entity.is_empty() {
                continue;
            }
            by_entity.entry(entity).or_default().push(CandidateTarget {
                object_key: obj.object_key.clone(),
                namespace: obj.namespace.clone(),
                object_name: obj.name.clone(),
                identity_column,
                identity_type,
                identity_is_specific,
            });
        }
        Self { by_entity }
    }

    fn lookup(&self, entity: &str) -> Vec<CandidateTarget> {
        self.by_entity.get(entity).cloned().unwrap_or_default()
    }
}

/// An inferred FK-like edge ready to persist into `catalog_relationship`.
#[derive(Debug, Clone, PartialEq)]
pub struct InferredEdge {
    pub from_object_key: String,
    pub from_column: String,
    pub to_object_key: String,
    pub to_namespace: Option<String>,
    pub to_name: String,
    pub to_column: String,
    pub confidence: f64,
    pub relationship_type: String,
}

/// The stable relationship-type token for name-inferred references.
pub const NAME_INFERRED_REFERENCE: &str = "NameInferredReference";

/// Infer outgoing FK-like edges for one source object against the candidate
/// index. `declared_fk_columns` carries the lower-cased source column names that
/// already participate in a declared FK, so we never shadow a real key.
pub fn infer_relationships(
    source: &InferenceObject,
    targets: &CandidateTargetIndex,
    declared_fk_columns: &[String],
    options: &InferenceOptions,
) -> Vec<InferredEdge> {
    if !source.is_table_like {
        return Vec::new();
    }

    let declared: Vec<String> = declared_fk_columns
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect();

    let mut edges: Vec<InferredEdge> = Vec::new();
    let mut emitted_columns: Vec<String> = Vec::new();

    for column in &source.columns {
        let col_lower = column.name.to_ascii_lowercase();

        // Already covered by a declared FK — declared keys are authoritative.
        if declared.contains(&col_lower) {
            continue;
        }
        // Only consider columns that look like references (end in id/key/code).
        let Some(entity) = referenced_entity_from_column(&column.name) else {
            continue;
        };

        let mut candidates = targets.lookup(&entity);
        // Never infer a self-edge in the first pass.
        candidates.retain(|t| t.object_key != source.object_key);
        if candidates.is_empty() {
            continue;
        }

        let pre_filter_count = candidates.len();

        // Prefer same-namespace targets when present.
        let same_ns: Vec<CandidateTarget> = candidates
            .iter()
            .filter(|t| namespace_eq(&t.namespace, &source.namespace))
            .cloned()
            .collect();
        if !same_ns.is_empty() {
            candidates = same_ns;
        }

        // Still ambiguous after same-namespace filtering — skip rather than guess.
        if candidates.len() != 1 {
            continue;
        }
        let target = &candidates[0];

        // Require compatible types when both are known.
        if !types_compatible(column.type_name.as_deref(), target.identity_type.as_deref()) {
            continue;
        }

        let confidence = score_inference(source, column, target, pre_filter_count, options);
        if confidence < options.min_confidence {
            continue;
        }

        // One inferred edge per source column.
        if emitted_columns.contains(&col_lower) {
            continue;
        }
        emitted_columns.push(col_lower);

        edges.push(InferredEdge {
            from_object_key: source.object_key.clone(),
            from_column: column.name.clone(),
            to_object_key: target.object_key.clone(),
            to_namespace: target.namespace.clone(),
            to_name: target.object_name.clone(),
            to_column: target.identity_column.clone(),
            confidence,
            relationship_type: NAME_INFERRED_REFERENCE.to_string(),
        });
    }

    edges
}

fn score_inference(
    source: &InferenceObject,
    column: &InferenceColumn,
    target: &CandidateTarget,
    pre_filter_count: usize,
    options: &InferenceOptions,
) -> f64 {
    // Base: exact entity-name match (the only way we got here).
    let mut score = 0.50;

    if namespace_eq(&source.namespace, &target.namespace) {
        score += options.same_namespace_bonus;
    }
    if both_known_and_compatible(column.type_name.as_deref(), target.identity_type.as_deref()) {
        score += options.type_match_bonus;
    }
    if target.identity_is_specific {
        score += options.exact_identity_bonus;
    }
    if pre_filter_count > 1 {
        score -= options.ambiguity_penalty * (pre_filter_count.saturating_sub(1) as f64);
    }
    if is_audit_column(&column.name) {
        score -= options.audit_column_penalty;
    }

    score.clamp(0.0, 1.0)
}

/// Lower-case and strip separators (`_`, `-`, spaces) so naming styles collapse.
pub fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '_' && *c != '-' && *c != ' ')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Very small, deterministic singularizer. Not linguistically complete — just
/// enough for common table-naming conventions. Input must already be normalized.
pub fn singularize(s: &str) -> String {
    if s.len() <= 2 {
        return s.to_string();
    }
    if let Some(stem) = s.strip_suffix("ies") {
        // companies -> company, policies -> policy
        return format!("{stem}y");
    }
    for suffix in ["ses", "xes", "zes", "ches", "shes"] {
        if let Some(stem) = s.strip_suffix(suffix) {
            // statuses -> status, boxes -> box, batches -> batch
            return format!("{stem}{}", &suffix[..suffix.len() - 2]);
        }
    }
    if s.ends_with("ss") || s.ends_with("us") || s.ends_with("is") {
        // address, status, campus, analysis: not plurals.
        return s.to_string();
    }
    if let Some(stem) = s.strip_suffix('s') {
        // orders -> order, customers -> customer
        return stem.to_string();
    }
    s.to_string()
}

/// Canonical entity key for a target table name (normalized + singularized).
fn entity_key_from_table(name: &str) -> String {
    singularize(&normalize(name))
}

/// Derive the referenced entity from a column name, stripping a trailing
/// `id`/`key`/`code` suffix. Returns `None` for columns that don't look like
/// references. Result is normalized + singularized to match target entity keys.
fn referenced_entity_from_column(column: &str) -> Option<String> {
    let norm = normalize(column);
    for suffix in ["id", "key", "code"] {
        if let Some(stem) = norm.strip_suffix(suffix) {
            if stem.is_empty() {
                // Bare `id`/`key`/`code` references nothing specific.
                return None;
            }
            return Some(singularize(stem));
        }
    }
    None
}

/// Choose the best identity column for a target object, in priority order.
/// Returns `(column_name, type_name, is_specific)`.
fn pick_identity_column(obj: &InferenceObject) -> Option<(String, Option<String>, bool)> {
    let entity = singularize(&normalize(&obj.name));

    // Build a lookup of normalized column name -> (actual name, type).
    let cols: Vec<(String, &InferenceColumn)> = obj
        .columns
        .iter()
        .map(|c| (normalize(&c.name), c))
        .collect();

    let find = |needle: &str| -> Option<&(String, &InferenceColumn)> {
        cols.iter().find(|(n, _)| n == needle)
    };

    // Priority: specific <entity>id, then generic id, then key/code variants.
    let specific_id = format!("{entity}id");
    let specific_key = format!("{entity}key");
    let specific_code = format!("{entity}code");

    let priorities: [(&str, bool); 7] = [
        (specific_id.as_str(), true),
        ("id", false),
        (specific_key.as_str(), true),
        ("key", false),
        (specific_code.as_str(), true),
        ("code", false),
        // A column literally normalized to the singular entity name (rare).
        (entity.as_str(), false),
    ];

    for (needle, is_specific) in priorities {
        if needle.is_empty() {
            continue;
        }
        if let Some((_, col)) = find(needle) {
            return Some((col.name.clone(), col.type_name.clone(), is_specific));
        }
    }
    None
}

/// Whether a column is a low-value audit/bookkeeping reference.
fn is_audit_column(column: &str) -> bool {
    let n = normalize(column);
    matches!(
        n.as_str(),
        "createdbyid"
            | "updatedbyid"
            | "modifiedbyid"
            | "deletedbyid"
            | "createdby"
            | "updatedby"
            | "modifiedby"
            | "deletedby"
    )
}

fn namespace_eq(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        (None, None) => true,
        _ => false,
    }
}

/// Type compatibility used as a hard gate. Permissive when either side is
/// unknown; otherwise requires normalized base-type equality.
fn types_compatible(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => base_type(x) == base_type(y),
        _ => true,
    }
}

/// Stricter form used only for the scoring bonus: both must be present AND equal.
fn both_known_and_compatible(a: Option<&str>, b: Option<&str>) -> bool {
    matches!((a, b), (Some(x), Some(y)) if base_type(x) == base_type(y))
}

/// Canonical base type: lower-cased, parameters/length stripped, common
/// integer/uuid/string synonyms folded together.
pub(crate) fn base_type(t: &str) -> String {
    let lower = t.trim().to_ascii_lowercase();
    let head: String = lower
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ' ')
        .collect();
    let head = head.trim();
    match head {
        "int" | "integer" | "int4" | "int8" | "bigint" | "smallint" | "tinyint" | "serial"
        | "bigserial" | "mediumint" => "int".to_string(),
        "uuid" | "uniqueidentifier" => "uuid".to_string(),
        "varchar" | "nvarchar" | "char" | "nchar" | "text" | "ntext" | "string"
        | "character varying" | "character" => "text".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: Option<&str>) -> InferenceColumn {
        InferenceColumn {
            name: name.to_string(),
            type_name: ty.map(|s| s.to_string()),
        }
    }

    fn obj(ns: Option<&str>, name: &str, cols: &[InferenceColumn]) -> InferenceObject {
        InferenceObject {
            object_key: format!(
                "{}.{}",
                ns.unwrap_or("").to_ascii_lowercase(),
                name.to_ascii_lowercase()
            ),
            namespace: ns.map(|s| s.to_string()),
            name: name.to_string(),
            is_table_like: true,
            columns: cols.to_vec(),
        }
    }

    #[test]
    fn normalize_strips_separators_and_case() {
        assert_eq!(normalize("Reporting_Period-Id"), "reportingperiodid");
        assert_eq!(normalize("company id"), "companyid");
    }

    #[test]
    fn singularize_handles_common_plurals() {
        assert_eq!(singularize("companies"), "company");
        assert_eq!(singularize("statuses"), "status");
        assert_eq!(singularize("orders"), "order");
        assert_eq!(singularize("customers"), "customer");
        assert_eq!(singularize("boxes"), "box");
        // Non-plural words are unchanged.
        assert_eq!(singularize("status"), "status");
        assert_eq!(singularize("address"), "address");
    }

    #[test]
    fn referenced_entity_strips_suffixes() {
        assert_eq!(
            referenced_entity_from_column("ReportingPeriodId").as_deref(),
            Some("reportingperiod")
        );
        assert_eq!(
            referenced_entity_from_column("reporting_period_id").as_deref(),
            Some("reportingperiod")
        );
        assert_eq!(
            referenced_entity_from_column("CustomerCode").as_deref(),
            Some("customer")
        );
        // Plural column stem singularizes to match singular table entity.
        assert_eq!(
            referenced_entity_from_column("OrdersId").as_deref(),
            Some("order")
        );
        // Bare id references nothing.
        assert_eq!(referenced_entity_from_column("Id"), None);
        // Non-reference columns.
        assert_eq!(referenced_entity_from_column("Quantity"), None);
    }

    #[test]
    fn pick_identity_prefers_specific_over_generic() {
        let t = obj(
            Some("acc"),
            "ReportingPeriod",
            &[
                col("Id", Some("int")),
                col("ReportingPeriodId", Some("int")),
                col("Name", Some("text")),
            ],
        );
        let (name, _ty, specific) = pick_identity_column(&t).unwrap();
        assert_eq!(name, "ReportingPeriodId");
        assert!(specific);
    }

    #[test]
    fn pick_identity_accepts_generic_id() {
        let t = obj(Some("dbo"), "Customer", &[col("Id", Some("int"))]);
        let (name, _ty, specific) = pick_identity_column(&t).unwrap();
        assert_eq!(name, "Id");
        assert!(!specific);
    }

    #[test]
    fn pick_identity_accepts_code() {
        let t = obj(Some("dbo"), "Currency", &[col("Code", Some("text"))]);
        let (name, _ty, _) = pick_identity_column(&t).unwrap();
        assert_eq!(name, "Code");
    }

    #[test]
    fn infers_same_schema_reference() {
        let source = obj(
            Some("acc"),
            "JournalLine",
            &[
                col("JournalLineId", Some("int")),
                col("ReportingPeriodId", Some("int")),
            ],
        );
        let target = obj(
            Some("acc"),
            "ReportingPeriod",
            &[
                col("ReportingPeriodId", Some("int")),
                col("Name", Some("text")),
            ],
        );
        let index = CandidateTargetIndex::build(&[source.clone(), target.clone()]);
        let edges = infer_relationships(&source, &index, &[], &InferenceOptions::default());
        assert_eq!(edges.len(), 1, "expected one inferred edge, got {edges:?}");
        let e = &edges[0];
        assert_eq!(e.to_object_key, "acc.reportingperiod");
        assert_eq!(e.from_column, "ReportingPeriodId");
        assert_eq!(e.to_column, "ReportingPeriodId");
        assert_eq!(e.relationship_type, NAME_INFERRED_REFERENCE);
        // base .50 + same-ns .10 + type .10 + specific-identity .10 = .80
        assert!(
            (e.confidence - 0.80).abs() < 1e-9,
            "conf was {}",
            e.confidence
        );
    }

    #[test]
    fn skips_when_target_missing() {
        let source = obj(
            Some("acc"),
            "JournalLine",
            &[col("ReportingPeriodId", Some("int"))],
        );
        let index = CandidateTargetIndex::build(std::slice::from_ref(&source));
        let edges = infer_relationships(&source, &index, &[], &InferenceOptions::default());
        assert!(edges.is_empty());
    }

    #[test]
    fn skips_declared_fk_columns() {
        let source = obj(
            Some("acc"),
            "JournalLine",
            &[col("ReportingPeriodId", Some("int"))],
        );
        let target = obj(
            Some("acc"),
            "ReportingPeriod",
            &[col("ReportingPeriodId", Some("int"))],
        );
        let index = CandidateTargetIndex::build(&[source.clone(), target]);
        let edges = infer_relationships(
            &source,
            &index,
            &["reportingperiodid".to_string()],
            &InferenceOptions::default(),
        );
        assert!(edges.is_empty(), "declared FK column must not be inferred");
    }

    #[test]
    fn skips_ambiguous_cross_schema_targets() {
        let source = obj(Some("sales"), "Invoice", &[col("UserId", Some("int"))]);
        let t1 = obj(Some("auth"), "User", &[col("UserId", Some("int"))]);
        let t2 = obj(Some("audit"), "User", &[col("UserId", Some("int"))]);
        let index = CandidateTargetIndex::build(&[source.clone(), t1, t2]);
        let edges = infer_relationships(&source, &index, &[], &InferenceOptions::default());
        assert!(
            edges.is_empty(),
            "two cross-schema User tables, neither in source ns -> ambiguous, skip"
        );
    }

    #[test]
    fn same_namespace_disambiguates() {
        let source = obj(Some("sales"), "Invoice", &[col("UserId", Some("int"))]);
        let same = obj(Some("sales"), "User", &[col("UserId", Some("int"))]);
        let other = obj(Some("audit"), "User", &[col("UserId", Some("int"))]);
        let index = CandidateTargetIndex::build(&[source.clone(), same, other]);
        let edges = infer_relationships(&source, &index, &[], &InferenceOptions::default());
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].to_object_key, "sales.user");
    }

    #[test]
    fn skips_on_type_mismatch() {
        let source = obj(Some("dbo"), "Order", &[col("CustomerId", Some("uuid"))]);
        let target = obj(Some("dbo"), "Customer", &[col("CustomerId", Some("int"))]);
        let index = CandidateTargetIndex::build(&[source.clone(), target]);
        let edges = infer_relationships(&source, &index, &[], &InferenceOptions::default());
        assert!(edges.is_empty(), "uuid vs int must not infer");
    }

    #[test]
    fn permissive_when_types_unknown() {
        let source = obj(Some("dbo"), "Order", &[col("CustomerId", None)]);
        let target = obj(Some("dbo"), "Customer", &[col("CustomerId", None)]);
        let index = CandidateTargetIndex::build(&[source.clone(), target]);
        let edges = infer_relationships(&source, &index, &[], &InferenceOptions::default());
        assert_eq!(edges.len(), 1, "unknown types are permissive");
    }

    #[test]
    fn audit_columns_are_penalized_below_threshold() {
        // created_by_id -> entity "createdby"? No: strips "id" -> "createdby".
        // Provide a User table whose entity matches "createdby"? It won't.
        // Instead test a direct audit reference that would otherwise pass.
        let source = obj(Some("dbo"), "Document", &[col("CreatedById", Some("int"))]);
        // Target entity must equal "createdby" to be a candidate; craft one.
        let target = obj(Some("dbo"), "CreatedBy", &[col("Id", Some("int"))]);
        let index = CandidateTargetIndex::build(&[source.clone(), target]);
        let edges = infer_relationships(&source, &index, &[], &InferenceOptions::default());
        // base .50 + same-ns .10 + type .10 + generic-id (no bonus) - audit .20 = .50 < .55
        assert!(
            edges.is_empty(),
            "audit column penalty should drop it below threshold, got {edges:?}"
        );
    }

    #[test]
    fn self_edges_are_skipped() {
        let source = obj(
            Some("dbo"),
            "Employee",
            &[
                col("EmployeeId", Some("int")),
                col("ManagerId", Some("int")),
            ],
        );
        // ManagerId -> entity "manager"; no Manager table, and Employee is self.
        let index = CandidateTargetIndex::build(std::slice::from_ref(&source));
        let edges = infer_relationships(&source, &index, &[], &InferenceOptions::default());
        assert!(edges.is_empty(), "no self-edges in first pass");
    }

    #[test]
    fn routines_are_not_targets_or_sources() {
        let mut routine = obj(Some("dbo"), "DoThing", &[col("CustomerId", Some("int"))]);
        routine.is_table_like = false;
        let target = obj(Some("dbo"), "Customer", &[col("CustomerId", Some("int"))]);
        let index = CandidateTargetIndex::build(&[routine.clone(), target]);
        let edges = infer_relationships(&routine, &index, &[], &InferenceOptions::default());
        assert!(edges.is_empty(), "routines never infer");
    }

    #[test]
    fn base_type_folds_int_family() {
        assert_eq!(base_type("BIGINT"), "int");
        assert_eq!(base_type("int(11)"), "int");
        assert_eq!(base_type("smallint"), "int");
        assert_eq!(base_type("uniqueidentifier"), "uuid");
        assert_eq!(base_type("nvarchar(50)"), "text");
    }
}
