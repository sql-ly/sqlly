//! Validation (ADR-011 §5): tree scope (single tree in isolation) and model
//! scope (symbol resolution against the snapshot), the database-agnostic
//! issue taxonomy, ADR-010 slot-type rules generalized over `TypeClass`, and
//! fixed/edit modes with exemptions.

use serde::{Deserialize, Serialize};
use sqlly_lang::sqlmodel::{
    Dialect, Fidelity, ModelSnapshot, ObjectKind, ObjectRef, Span, SqlName, TypeClass, TypeRef,
};
use sqlly_lang::sqlparse::{
    LiteralShape, ParsedTree, Slot, SlotKind, SlotMember, StatementNode, UseKind, parse,
    resolve_slots_in_tree,
};

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

/// Database-agnostic issue classification (ADR-011 §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueClass {
    Syntax,
    UnresolvedObject,
    UnresolvedColumn,
    AmbiguousReference,
    TypeMismatch,
    TypeRisk,
    ArityMismatch,
    CapabilityViolation,
    ScopeInconsistency,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub class: IssueClass,
    /// Stable code, e.g. "SQM2001"; dialect-qualified refinements append a
    /// suffix ("SQM3002.mysql").
    pub code: String,
    pub span: Span,
    /// 1-based line of `span.start` in the validated text.
    pub line: u32,
    /// 1-based character position of `span.start` within its line.
    pub column: u32,
    pub message: String,
    pub dialect_note: Option<String>,
    pub related: Vec<Span>,
}

/// 1-based (line, column) of a byte offset in `text`.
pub fn line_column(text: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.bytes().filter(|b| *b == b'\n').count() as u32 + 1;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let column = text[line_start..offset].chars().count() as u32 + 1;
    (line, column)
}

/// Fill in `line`/`column` for every diagnostic from the source text.
pub fn position_diagnostics(text: &str, diagnostics: &mut [Diagnostic]) {
    for d in diagnostics.iter_mut() {
        let (line, column) = line_column(text, d.span.start);
        d.line = line;
        d.column = column;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidationScope {
    Tree,
    Model,
}

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExemptionSet {
    /// Fold-key prefixes / file paths exempted from validation, with reasons.
    pub entries: Vec<Exemption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exemption {
    /// Matches an object when its fold key starts with this (or a file path
    /// exactly).
    pub pattern: String,
    pub reason: String,
}

impl ExemptionSet {
    pub fn exempts(&self, reference: &ObjectRef) -> bool {
        let key = reference.fold_key();
        let bare = reference.name.fold_key();
        self.entries
            .iter()
            .any(|e| key.starts_with(&e.pattern) || bare == e.pattern)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineMode {
    Fixed,
    Edit { exemptions: ExemptionSet },
}

impl EngineMode {
    pub fn exemptions(&self) -> Option<&ExemptionSet> {
        match self {
            EngineMode::Fixed => None,
            EngineMode::Edit { exemptions } => Some(exemptions),
        }
    }
}

// ---------------------------------------------------------------------------
// Query validation report (ADR-011 §5.4)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolResolution {
    pub reference: ObjectRef,
    pub span: Span,
    pub resolved_to: Option<ObjectRef>,
    pub kind: Option<ObjectKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryValidationReport {
    /// No Error-severity issues. TypeRisk warnings keep valid == true.
    pub valid: bool,
    pub issues: Vec<Diagnostic>,
    pub resolved: Vec<SymbolResolution>,
    pub snapshot_generation: u64,
}

pub struct QueryValidationRequest {
    pub text: String,
    pub dialect: Dialect,
    pub default_catalog: Option<SqlName>,
    pub default_namespace: Option<SqlName>,
    pub scope: ValidationScope,
}

// ---------------------------------------------------------------------------
// Tree-scope validation
// ---------------------------------------------------------------------------

pub fn validate_tree(tree: &ParsedTree, mode: &EngineMode) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    // Recovery actions: errors in fixed mode, hints in edit mode.
    let recovery_severity = match mode {
        EngineMode::Fixed => Severity::Error,
        EngineMode::Edit { .. } => Severity::Hint,
    };
    if let Fidelity::Recovered { actions } = tree.fidelity() {
        for action in actions {
            out.push(Diagnostic {
                severity: recovery_severity,
                class: IssueClass::Syntax,
                code: format!("SQM10{:02}", syntax_code(&action.kind)),
                span: action.span,
                line: 0,
                column: 0,
                message: format!("syntax recovered: {}", action.assumption),
                dialect_note: None,
                related: vec![],
            });
        }
    }

    for stmt in &tree.statements {
        if let StatementNode::CreateTable(node) = stmt {
            // Duplicate column names.
            let mut seen: std::collections::HashMap<&str, Span> = std::collections::HashMap::new();
            for col in &node.body.columns {
                if let Some(_prior) = seen.insert(col.name.fold_key(), node.span) {
                    out.push(Diagnostic {
                        severity: Severity::Error,
                        class: IssueClass::ScopeInconsistency,
                        code: "SQM1101".into(),
                        span: node.span,
                        line: 0,
                        column: 0,
                        message: format!(
                            "duplicate column name `{}` in `{}`",
                            col.name.spelling(),
                            node.reference.display()
                        ),
                        dialect_note: None,
                        related: vec![],
                    });
                }
            }
            // Key/FK columns must exist in the definition.
            let mut check_columns = |columns: &[SqlName], what: &str| {
                for c in columns {
                    if node.body.column(c).is_none() {
                        out.push(Diagnostic {
                            severity: Severity::Error,
                            class: IssueClass::ScopeInconsistency,
                            code: "SQM1102".into(),
                            span: node.span,
                            line: 0,
                            column: 0,
                            message: format!(
                                "{what} references column `{}` which is not defined in `{}`",
                                c.spelling(),
                                node.reference.display()
                            ),
                            dialect_note: None,
                            related: vec![],
                        });
                    }
                }
            };
            if let Some(pk) = &node.body.primary_key {
                check_columns(&pk.columns, "primary key");
            }
            for fk in &node.body.foreign_keys {
                check_columns(&fk.columns, "foreign key");
            }
            // Unknown type spellings (class Other with no custom resolution
            // is only a warning — could be a UDT resolved at model scope).
            for col in &node.body.columns {
                if let Some(ty) = &col.ty
                    && ty.class == TypeClass::Other
                {
                    out.push(Diagnostic {
                        severity: Severity::Warning,
                        class: IssueClass::TypeRisk,
                        code: "SQM1103".into(),
                        span: node.span,
                        line: 0,
                        column: 0,
                        message: format!(
                            "type `{}` of column `{}` is not a recognized {} type; \
                                 it must resolve to a user-defined type",
                            ty.native,
                            col.name.spelling(),
                            dialect_name(tree.dialect)
                        ),
                        dialect_note: None,
                        related: vec![],
                    });
                }
            }
        }
    }
    out
}

fn syntax_code(kind: &str) -> u8 {
    match kind {
        "AssumedComma" => 1,
        "MissingType" => 2,
        "UnterminatedStringClosed" => 3,
        "UnbalancedParenClosed" => 4,
        "GrabBag" => 5,
        _ => 9,
    }
}

fn dialect_name(d: Dialect) -> &'static str {
    match d {
        Dialect::SqlServer => "SQL Server",
        Dialect::Postgres => "PostgreSQL",
        Dialect::MySql => "MySQL",
        Dialect::Oracle => "Oracle",
        Dialect::Sqlite => "SQLite",
    }
}

// ---------------------------------------------------------------------------
// Model-scope validation
// ---------------------------------------------------------------------------

pub struct ResolutionContext<'a> {
    pub snapshot: &'a ModelSnapshot,
    pub default_catalog: Option<&'a SqlName>,
    pub default_namespace: Option<&'a SqlName>,
    pub mode: &'a EngineMode,
}

pub fn validate_model(
    tree: &ParsedTree,
    ctx: &ResolutionContext<'_>,
) -> (Vec<Diagnostic>, Vec<SymbolResolution>) {
    let mut issues = Vec::new();
    let mut resolutions = Vec::new();

    for use_ in &tree.references {
        // Column references are not model objects. Qualified columns are
        // checked against in-statement aliases below; bare predicate columns
        // are consumed by slot/type analysis when they can be bound, and stay
        // conservative when they cannot.
        if matches!(use_.use_kind, UseKind::Column | UseKind::ColumnBare) {
            continue;
        }
        let exempt = ctx
            .mode
            .exemptions()
            .map(|e| e.exempts(&use_.reference))
            .unwrap_or(false);
        let candidates =
            ctx.snapshot
                .resolve(&use_.reference, ctx.default_catalog, ctx.default_namespace);
        let wanted_kinds: &[ObjectKind] = match use_.use_kind {
            UseKind::TableSource => &[
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::Synonym,
                ObjectKind::TableFunction,
            ],
            UseKind::RoutineCall => &[
                ObjectKind::Procedure,
                ObjectKind::Function,
                ObjectKind::ScalarFunction,
                ObjectKind::TableFunction,
                ObjectKind::AggregateFunction,
            ],
            UseKind::ForeignKeyTarget => &[ObjectKind::Table],
            _ => &[],
        };
        let matching: Vec<_> = if wanted_kinds.is_empty() {
            candidates.clone()
        } else {
            candidates
                .iter()
                .filter(|e| wanted_kinds.contains(&e.kind))
                .cloned()
                .collect()
        };

        match matching.len() {
            0 => {
                resolutions.push(SymbolResolution {
                    reference: use_.reference.clone(),
                    span: use_.span,
                    resolved_to: None,
                    kind: None,
                });
                if exempt {
                    continue;
                }
                if ctx.snapshot.incomplete.covers(&use_.reference) {
                    issues.push(Diagnostic {
                        severity: Severity::Hint,
                        class: IssueClass::Incomplete,
                        code: "SQM2003".into(),
                        span: use_.span,
                        line: 0,
                        column: 0,
                        message: format!(
                            "`{}` could not be checked: its region failed to load",
                            use_.reference.display()
                        ),
                        dialect_note: None,
                        related: vec![],
                    });
                } else {
                    issues.push(Diagnostic {
                        severity: Severity::Error,
                        class: IssueClass::UnresolvedObject,
                        code: "SQM2001".into(),
                        span: use_.span,
                        line: 0,
                        column: 0,
                        message: format!(
                            "`{}` does not exist in the current model",
                            use_.reference.display()
                        ),
                        dialect_note: None,
                        related: vec![],
                    });
                }
            }
            1 => {
                resolutions.push(SymbolResolution {
                    reference: use_.reference.clone(),
                    span: use_.span,
                    resolved_to: Some(matching[0].reference.clone()),
                    kind: Some(matching[0].kind),
                });
            }
            _ => {
                resolutions.push(SymbolResolution {
                    reference: use_.reference.clone(),
                    span: use_.span,
                    resolved_to: Some(matching[0].reference.clone()),
                    kind: Some(matching[0].kind),
                });
                if !exempt && use_.reference.namespace.is_none() {
                    issues.push(Diagnostic {
                        severity: Severity::Warning,
                        class: IssueClass::AmbiguousReference,
                        code: "SQM2002".into(),
                        span: use_.span,
                        line: 0,
                        column: 0,
                        message: format!(
                            "`{}` matches more than one object; qualify the name",
                            use_.reference.display()
                        ),
                        dialect_note: None,
                        related: vec![],
                    });
                }
            }
        }
    }
    validate_column_qualifiers(tree, ctx, &mut issues);
    (issues, resolutions)
}

/// Flag qualified column references (`alias.column`) whose qualifier matches no
/// table source in the statement. The set of valid qualifiers is the union of
/// every table-source alias and every table name brought into scope by a
/// FROM/JOIN/UPDATE/DELETE/INSERT target. A qualifier that matches neither is an
/// undefined alias: e.g. `t.test` when only `m`/`ml` are in scope.
///
/// Schema names are also treated as valid qualifiers so that schema-qualified
/// references that the two-part scan happens to catch (e.g. `dbo.SomeFn(...)`)
/// are not flagged as undefined aliases.
fn validate_column_qualifiers(
    tree: &ParsedTree,
    ctx: &ResolutionContext<'_>,
    issues: &mut Vec<Diagnostic>,
) {
    use std::collections::HashSet;

    // Gather every alias and table name introduced as a table source. Compared
    // case-insensitively via fold keys, matching SQL's identifier semantics.
    let mut known: HashSet<String> = HashSet::new();
    for use_ in &tree.references {
        if use_.use_kind != UseKind::TableSource {
            continue;
        }
        if let Some(alias) = &use_.via_alias {
            known.insert(alias.to_ascii_lowercase());
        }
        // The bare table name is also a valid qualifier when no alias is used.
        known.insert(use_.reference.name.fold_key().to_string());
    }

    // If there are no table sources at all, we cannot meaningfully resolve
    // qualifiers; stay silent rather than flag everything.
    if known.is_empty() {
        return;
    }

    // Schema/namespace names are valid qualifiers too (e.g. `dbo.Fn(...)`).
    for entry in ctx.snapshot.objects() {
        if let Some(ns) = &entry.reference.namespace {
            known.insert(ns.fold_key().to_string());
        }
    }
    if let Some(ns) = ctx.default_namespace {
        known.insert(ns.fold_key().to_string());
    }

    for use_ in &tree.references {
        if use_.use_kind != UseKind::Column {
            continue;
        }
        let Some(alias) = &use_.via_alias else {
            continue;
        };
        let key = alias.to_ascii_lowercase();
        if known.contains(&key) {
            continue;
        }
        issues.push(Diagnostic {
            severity: Severity::Error,
            class: IssueClass::UnresolvedColumn,
            code: "SQM2004".into(),
            span: use_.span,
            line: 0,
            column: 0,
            message: format!("`{alias}` is not a defined table or alias in this statement"),
            dialect_note: None,
            related: vec![],
        });
    }
}

// ---------------------------------------------------------------------------
// Slot-type pass (ADR-010 generalized; ADR-011 §5.4 step 2)
// ---------------------------------------------------------------------------

pub fn validate_slots(
    tree: &ParsedTree,
    text: &str,
    ctx: &ResolutionContext<'_>,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let slots = resolve_slots_in_tree(tree, text, tree.dialect);

    for slot in &slots {
        if ctx
            .mode
            .exemptions()
            .map(|e| e.exempts(&slot.container))
            .unwrap_or(false)
        {
            continue;
        }
        // Resolve the slot's type from the model.
        let ty = match resolve_slot_type(slot, ctx) {
            Some(t) => t,
            None => continue, // conservative: unresolvable slot → no diagnostic
        };
        check_literal(slot, &ty, tree.dialect, &mut out);
    }

    // INSERT arity check (ADR-011 §5.4 step 1).
    for stmt in &tree.statements {
        if let StatementNode::Insert(node) = stmt {
            if node.columns.is_empty() {
                continue;
            }
            for (row, arity) in node.value_row_arities.iter().enumerate() {
                if *arity != node.columns.len() {
                    out.push(Diagnostic {
                        severity: Severity::Error,
                        class: IssueClass::ArityMismatch,
                        code: "SQM4001".into(),
                        span: node.span,
                        line: 0,
                        column: 0,
                        message: format!(
                            "INSERT row {} provides {} value(s) for {} column(s)",
                            row + 1,
                            arity,
                            node.columns.len()
                        ),
                        dialect_note: None,
                        related: node.columns.iter().map(|(_, s)| *s).collect(),
                    });
                }
            }
        }
    }
    out
}

fn resolve_slot_type(slot: &Slot, ctx: &ResolutionContext<'_>) -> Option<TypeRef> {
    match slot.kind {
        SlotKind::RoutineArg => {
            let routine = ctx
                .snapshot
                .resolve(&slot.container, ctx.default_catalog, ctx.default_namespace)
                .into_iter()
                .find(|e| {
                    matches!(
                        e.kind,
                        ObjectKind::Procedure
                            | ObjectKind::Function
                            | ObjectKind::ScalarFunction
                            | ObjectKind::TableFunction
                            | ObjectKind::AggregateFunction
                    )
                })?;
            let body = routine.routine()?;
            match &slot.member {
                SlotMember::Named(name) => body
                    .params
                    .iter()
                    .find(|p| p.name.fold_key() == name.fold_key())
                    .and_then(|p| p.ty.clone()),
                SlotMember::Positional(i) => body.params.get(*i).and_then(|p| p.ty.clone()),
            }
        }
        SlotKind::InsertValue | SlotKind::UpdateAssign | SlotKind::Predicate => {
            let table = ctx.snapshot.resolve_kind(
                &slot.container,
                ObjectKind::Table,
                ctx.default_catalog,
                ctx.default_namespace,
            )?;
            let body = table.table()?;
            match &slot.member {
                SlotMember::Named(name) => body
                    .column_by_fold(name.fold_key())
                    .and_then(|c| c.ty.clone()),
                SlotMember::Positional(i) => body.columns.get(*i).and_then(|c| c.ty.clone()),
            }
        }
        SlotKind::VariableAssign => None,
    }
}

/// ADR-010 type rules over canonical classes, with dialect refinements.
fn check_literal(slot: &Slot, ty: &TypeRef, dialect: Dialect, out: &mut Vec<Diagnostic>) {
    use LiteralShape as L;
    let v = &slot.value;
    // Parameters, NULLs, missing values, bare words (columns/functions) are
    // never literal-validated.
    if matches!(
        v.shape,
        L::Parameter | L::Null | L::Missing | L::Word | L::Other
    ) {
        return;
    }
    // IS/IN/BETWEEN operate on sets/ranges; only check scalar comparisons,
    // assignments, and routine args.
    if matches!(slot.operator.as_deref(), Some("is") | Some("in")) {
        return;
    }

    // Fully qualify the target so the message names exactly what the value
    // feeds: `Accounting.PurchaseOrder.PaymentMethodId`, not just "the type".
    let target = match &slot.member {
        SlotMember::Named(name) => {
            format!("{}.{}", slot.container.display(), name.spelling())
        }
        SlotMember::Positional(_) => {
            format!("{} {}", slot.container.display(), slot.member.display())
        }
    };
    let push = |out: &mut Vec<Diagnostic>,
                severity: Severity,
                class: IssueClass,
                code: &str,
                message: String,
                dialect_note: Option<String>| {
        out.push(Diagnostic {
            severity,
            class,
            code: code.into(),
            span: v.span,
            line: 0,
            column: 0,
            message,
            dialect_note,
            related: vec![slot.member_span],
        });
    };

    match ty.class {
        TypeClass::Integer => match v.shape {
            L::String { .. } => {
                // Dialect refinement: SQL Server bit accepts 'true'/'false'.
                let unquoted = v.unquoted().to_ascii_lowercase();
                let mssql_bit = dialect == Dialect::SqlServer
                    && ty.native.to_ascii_lowercase().starts_with("bit")
                    && matches!(unquoted.as_str(), "true" | "false");
                if mssql_bit {
                    return;
                }
                // Numeric-in-string is implicit-convertible but lossy style.
                if unquoted.parse::<i128>().is_ok() {
                    push(
                        out,
                        Severity::Warning,
                        IssueClass::TypeRisk,
                        "SQM3004",
                        format!(
                            "string literal {} passed to integer target `{}` ({}); \
                             relies on implicit conversion",
                            v.text, target, ty.native
                        ),
                        None,
                    );
                } else {
                    push(
                        out,
                        Severity::Error,
                        IssueClass::TypeMismatch,
                        "SQM3001",
                        format!(
                            "string literal {} is not valid for integer target `{}` ({})",
                            v.text, target, ty.native
                        ),
                        None,
                    );
                }
            }
            L::Decimal => {
                push(
                    out,
                    Severity::Warning,
                    IssueClass::TypeRisk,
                    "SQM3002",
                    format!(
                        "fractional value {} will be truncated by integer target `{}` ({})",
                        v.text, target, ty.native
                    ),
                    None,
                );
            }
            L::Integer | L::Hex | L::Scientific => {
                if let (Some((lo, hi)), Ok(parsed)) =
                    (ty.integer_range_for(dialect), parse_integer(&v.text))
                    && (parsed < lo || parsed > hi)
                {
                    push(
                        out,
                        Severity::Warning,
                        IssueClass::TypeRisk,
                        "SQM3003",
                        format!(
                            "value {} is outside the range of `{}` ({}..={}) for `{}`",
                            v.text, ty.native, lo, hi, target
                        ),
                        None,
                    );
                }
            }
            _ => {}
        },
        TypeClass::Decimal | TypeClass::Float => {
            if let L::String { .. } = v.shape {
                let unquoted = v.unquoted();
                if unquoted.parse::<f64>().is_ok() {
                    push(
                        out,
                        Severity::Warning,
                        IssueClass::TypeRisk,
                        "SQM3004",
                        format!(
                            "string literal {} passed to numeric target `{}` ({})",
                            v.text, target, ty.native
                        ),
                        None,
                    );
                } else {
                    push(
                        out,
                        Severity::Error,
                        IssueClass::TypeMismatch,
                        "SQM3001",
                        format!(
                            "string literal {} is not valid for numeric target `{}` ({})",
                            v.text, target, ty.native
                        ),
                        None,
                    );
                }
            }
        }
        TypeClass::Uuid => match v.shape {
            L::String { .. } => {
                if !is_uuid_shaped(&v.unquoted()) {
                    push(
                        out,
                        Severity::Error,
                        IssueClass::TypeMismatch,
                        "SQM3005",
                        format!(
                            "{} is not a valid GUID for target `{}` ({})",
                            v.text, target, ty.native
                        ),
                        None,
                    );
                }
            }
            L::Integer | L::Decimal | L::Hex | L::Scientific => {
                push(
                    out,
                    Severity::Error,
                    IssueClass::TypeMismatch,
                    "SQM3005",
                    format!(
                        "numeric literal {} is not valid for GUID target `{}` ({})",
                        v.text, target, ty.native
                    ),
                    None,
                );
            }
            _ => {}
        },
        TypeClass::Boolean => {
            if let L::String { .. } = v.shape {
                // Postgres accepts 't'/'f'/'true'/'false' strings; MySQL not.
                let s = v.unquoted().to_ascii_lowercase();
                let ok_pg = matches!(
                    s.as_str(),
                    "t" | "f" | "true" | "false" | "yes" | "no" | "on" | "off" | "1" | "0"
                );
                if dialect == Dialect::Postgres && ok_pg {
                    return;
                }
                push(
                    out,
                    Severity::Warning,
                    IssueClass::TypeRisk,
                    &format!("SQM3006.{}", dialect_suffix(dialect)),
                    format!("string literal {} for boolean target `{}`", v.text, target),
                    Some(match dialect {
                        Dialect::Postgres => {
                            "PostgreSQL only accepts boolean-shaped strings".into()
                        }
                        _ => format!("{} prefers TRUE/FALSE or 1/0 here", dialect_name(dialect)),
                    }),
                );
            }
        }
        TypeClass::DateTime => {
            if let L::String { .. } = v.shape {
                // MySQL zero-date refinement.
                if dialect == Dialect::MySql && v.unquoted().starts_with("0000-00-00") {
                    push(
                        out,
                        Severity::Warning,
                        IssueClass::TypeRisk,
                        "SQM3007.mysql",
                        format!("zero date {} passed to `{}`", v.text, target),
                        Some(
                            "MySQL zero dates are rejected under strict SQL modes (NO_ZERO_DATE)"
                                .into(),
                        ),
                    );
                }
            } else if matches!(v.shape, L::Integer | L::Decimal) && dialect != Dialect::Sqlite {
                // SQLite stores datetimes as numbers legitimately; elsewhere
                // a bare number for a datetime is suspicious.
                push(
                    out,
                    Severity::Warning,
                    IssueClass::TypeRisk,
                    "SQM3008",
                    format!(
                        "numeric literal {} passed to date/time target `{}` ({})",
                        v.text, target, ty.native
                    ),
                    None,
                );
            }
        }
        // Character/binary/json/xml/spatial accept strings; everything else
        // stays silent (conservative).
        _ => {}
    }
}

fn parse_integer(text: &str) -> Result<i128, ()> {
    let t = text.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i128::from_str_radix(hex, 16).map_err(|_| ());
    }
    if let Ok(v) = t.parse::<i128>() {
        return Ok(v);
    }
    // Scientific that lands on an integer (1e3).
    if let Ok(f) = t.parse::<f64>()
        && f.fract() == 0.0
        && f.abs() < i128::MAX as f64
    {
        return Ok(f as i128);
    }
    Err(())
}

fn is_uuid_shaped(s: &str) -> bool {
    let s = s.trim_start_matches('{').trim_end_matches('}');
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8usize, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(len, p)| p.len() == *len && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn dialect_suffix(d: Dialect) -> &'static str {
    match d {
        Dialect::SqlServer => "mssql",
        Dialect::Postgres => "pg",
        Dialect::MySql => "mysql",
        Dialect::Oracle => "oracle",
        Dialect::Sqlite => "sqlite",
    }
}

// ---------------------------------------------------------------------------
// Full query validation (ADR-011 §5.4 pipeline)
// ---------------------------------------------------------------------------

pub fn validate_query(
    req: &QueryValidationRequest,
    snapshot: &ModelSnapshot,
    mode: &EngineMode,
) -> QueryValidationReport {
    let tree = parse(&req.text, req.dialect);
    let mut issues = validate_tree(&tree, mode);
    let mut resolved = Vec::new();

    if req.scope == ValidationScope::Model {
        let ctx = ResolutionContext {
            snapshot,
            default_catalog: req.default_catalog.as_ref(),
            default_namespace: req.default_namespace.as_ref(),
            mode,
        };
        let (model_issues, resolutions) = validate_model(&tree, &ctx);
        issues.extend(model_issues);
        resolved = resolutions;
        issues.extend(validate_slots(&tree, &req.text, &ctx));
    }

    issues.sort_by_key(|d| (d.span.start, d.span.end));
    position_diagnostics(&req.text, &mut issues);
    let valid = !issues.iter().any(|d| d.severity == Severity::Error);
    QueryValidationReport {
        valid,
        issues,
        resolved,
        snapshot_generation: snapshot.generation,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_integer;
    use sqlly_lang::sqlmodel::{Dialect, TypeRef};

    #[test]
    fn tinyint_integer_range_is_dialect_aware() {
        let mysql = TypeRef::parse("tinyint", Dialect::MySql);
        let (lo, hi) = mysql.integer_range_for(Dialect::MySql).unwrap();
        let parsed = parse_integer("-1").unwrap();
        assert!(
            (lo..=hi).contains(&parsed),
            "MySQL signed tinyint accepts -1"
        );

        let mssql = TypeRef::parse("tinyint", Dialect::SqlServer);
        let (lo, hi) = mssql.integer_range_for(Dialect::SqlServer).unwrap();
        let parsed = parse_integer("-1").unwrap();
        assert!(parsed < lo || parsed > hi, "SQL Server tinyint rejects -1");
    }
}
