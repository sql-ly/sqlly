//! Hover information for the identifier under a byte offset. Pure and
//! synchronous: resolution reuses `goto::resolve_symbol_at` and reads typed
//! details from the `ModelSnapshot`. No I/O, no per-keystroke work — the
//! editor calls this only after a hover dwell.

use sqlly_lang::sqlmodel::{
    Dialect, ModelSnapshot, ObjectBody, ObjectKind, ObjectRef, ParamMode, SqlName, TableBody,
    ViewBody,
};

use crate::intellisense::builtins::{BuiltinFunction, WordDoc, builtins};
use crate::intellisense::complete::scan;
use crate::intellisense::goto::{
    ClickedRole, NavigationTarget, RoutineKind, SymbolResolution, resolve_symbol_at,
    token_at_offset,
};
use crate::intellisense::types::{CompletionCatalog, CompletionObject};

/// Maximum columns listed in a table card before "… and N more".
pub const MAX_HOVER_COLUMNS: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverColumn {
    pub name: String,
    /// Native type text (`nvarchar(50)`), empty when unknown.
    pub data_type: String,
    /// `None` when nullability is unknown.
    pub nullable: Option<bool>,
    pub is_primary_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverParameter {
    pub name: String,
    pub data_type: String,
    pub is_output: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoverInfo {
    Table {
        qualified_name: String,
        is_view: bool,
        columns: Vec<HoverColumn>,
        more_columns: usize,
    },
    Column {
        table_qualified_name: String,
        column: HoverColumn,
    },
    Routine {
        qualified_name: String,
        kind: RoutineKind,
        parameters: Vec<HoverParameter>,
        returns: Option<String>,
    },
    /// A built-in function of the connection's dialect.
    Builtin(Box<BuiltinFunction>),
    /// A literal constant (`NULL`, `TRUE`), or a fixed word in a function's
    /// argument slot (`day` in `DATEADD(day, …)`) — then `slot` names where.
    Constant { doc: WordDoc, slot: Option<String> },
    /// A built-in data type.
    DataType(WordDoc),
    /// A server system variable read as `@@name` (MySQL/MariaDB).
    SystemVariable(WordDoc),
    /// A Redis command (or container subcommand) on a command-engine
    /// connection: documented syntax, summary, and safety badges.
    Command(Box<crate::redis_complete::CommandDoc>),
}

/// Procedure signatures the host already holds (mirrors
/// `sqlly_services::intellisense::ProcedureInfo` without the dependency).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineSignature {
    pub schema: String,
    pub name: String,
    pub parameters: Vec<HoverParameter>,
}

pub fn hover_at(
    sql: &str,
    offset: usize,
    catalog: &CompletionCatalog,
    snapshot: &ModelSnapshot,
    routines: &[RoutineSignature],
    default_schema: &str,
    default_database: Option<&str>,
) -> Option<(HoverInfo, (usize, usize))> {
    let SymbolResolution::Resolved(target) =
        resolve_symbol_at(sql, offset, catalog, default_schema, default_database)
    else {
        return None;
    };
    if matches!(target, NavigationTarget::Type { .. }) {
        return None;
    }

    let tokens = scan::scan_tokens(sql);
    let clicked = token_at_offset(&tokens, offset)?;
    let span = (tokens[clicked].byte_offset, tokens[clicked].end_byte);

    let dialect = Dialect::SqlServer;
    let reference = ObjectRef::new(
        target.catalog().map(|c| SqlName::new(c, dialect, false)),
        Some(SqlName::new(target.schema(), dialect, false)),
        SqlName::new(target.name(), dialect, false),
    );
    let entries = snapshot.resolve(&reference, None, None);
    let entry = entries.into_iter().find(|e| kind_matches(e.kind, &target));

    let info = match &target {
        NavigationTarget::Table { clicked, .. } => match clicked {
            ClickedRole::Column { name } => {
                let column = column_hover(&target, name, entry.map(|e| e.as_ref()), catalog);
                HoverInfo::Column {
                    table_qualified_name: target.qualified_name(),
                    column,
                }
            }
            ClickedRole::Table | ClickedRole::View | ClickedRole::Index { .. } => {
                let is_view = matches!(clicked, ClickedRole::View)
                    || entry.is_some_and(|e| {
                        matches!(e.kind, ObjectKind::View | ObjectKind::MaterializedView)
                    });
                table_hover(&target, is_view, entry.map(|e| e.as_ref()), catalog)
            }
            _ => return None,
        },
        NavigationTarget::Routine { kind, .. } => {
            routine_hover(&target, *kind, entry.map(|e| e.as_ref()), routines, catalog)
        }
        NavigationTarget::Type { .. } => return None,
    };

    Some((info, span))
}

/// Hover for a word of the dialect's own vocabulary — a built-in function, a
/// constant, a fixed word in a function's argument slot, or a data type.
/// Keywords get no card. The editor asks this only after the
/// schema-backed [`hover_at`] found nothing, so a user object that shares a
/// built-in's name keeps its own card. Delimited identifiers (`[name]`,
/// `"name"`) are names, never vocabulary.
pub fn builtin_hover_at(
    dialect: Dialect,
    sql: &str,
    offset: usize,
) -> Option<(HoverInfo, (usize, usize))> {
    let tokens = scan::scan_tokens(sql);
    let idx = token_at_offset(&tokens, offset)?;
    let token = &tokens[idx];
    let span = (token.byte_offset, token.end_byte);
    if matches!(
        sql.as_bytes().get(token.byte_offset),
        Some(b'[' | b'"' | b'`')
    ) {
        return None;
    }
    // A dotted name (`dbo.LEN`, `t.count`) is a user object or column.
    let dotted = |i: usize| {
        tokens
            .get(i)
            .is_some_and(|t| t.kind == scan::ScanTokenKind::Dot)
    };
    if (idx > 0 && dotted(idx - 1)) || dotted(idx + 1) {
        return None;
    }
    let word = token.text.as_str();
    let catalog = builtins(dialect);
    let called = tokens
        .get(idx + 1)
        .is_some_and(|t| t.kind == scan::ScanTokenKind::LeftParen);

    if called && let Some(f) = catalog.function(word) {
        return Some((HoverInfo::Builtin(Box::new(f.clone())), span));
    }
    // A fixed word in an argument slot: `DATEDIFF(minute, …)`.
    if let Some(ctx) = crate::intellisense::signature::call_context_at(sql, token.byte_offset)
        && !ctx.callee.contains('.')
        && let Some(param) = catalog.slot_param(&ctx.callee, ctx.active_parameter, word)
    {
        let function = catalog
            .function(&ctx.callee)
            .map(|f| f.name.clone())
            .unwrap_or(ctx.callee);
        return Some((
            HoverInfo::Constant {
                doc: WordDoc {
                    name: word.to_string(),
                    doc: param.doc.clone(),
                    syntax: None,
                },
                slot: Some(format!("{} argument of {function}", param.name)),
            },
            span,
        ));
    }
    if let Some(doc) = catalog.system_variable(word) {
        return Some((HoverInfo::SystemVariable(doc.clone()), span));
    }
    if let Some(doc) = catalog.constant(word) {
        return Some((
            HoverInfo::Constant {
                doc: doc.clone(),
                slot: None,
            },
            span,
        ));
    }
    // Keywords get no card: they are the words people know best, and a card
    // on every SELECT and FROM is noise.
    if catalog.is_keyword_word(&word.to_ascii_lowercase()) {
        return None;
    }
    if let Some(doc) = catalog.data_type(word) {
        return Some((HoverInfo::DataType(doc.clone()), span));
    }
    // A function named without its parentheses (`CURRENT_TIMESTAMP`, or a
    // call not typed yet).
    catalog
        .function(word)
        .map(|f| (HoverInfo::Builtin(Box::new(f.clone())), span))
}

fn kind_matches(kind: ObjectKind, target: &NavigationTarget) -> bool {
    match target {
        NavigationTarget::Table { .. } => {
            matches!(
                kind,
                ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
            )
        }
        NavigationTarget::Routine { kind: rk, .. } => match rk {
            RoutineKind::Procedure => matches!(kind, ObjectKind::Procedure),
            RoutineKind::Function => matches!(
                kind,
                ObjectKind::Function
                    | ObjectKind::ScalarFunction
                    | ObjectKind::TableFunction
                    | ObjectKind::AggregateFunction
            ),
        },
        NavigationTarget::Type { .. } => false,
    }
}

fn table_hover(
    target: &NavigationTarget,
    is_view: bool,
    entry: Option<&sqlly_lang::sqlmodel::ObjectEntry>,
    catalog: &CompletionCatalog,
) -> HoverInfo {
    if let Some(entry) = entry {
        match &entry.body {
            ObjectBody::Table(table) => {
                let (columns, more_columns) = hover_columns_from_table(table);
                return HoverInfo::Table {
                    qualified_name: target.qualified_name(),
                    is_view,
                    columns,
                    more_columns,
                };
            }
            ObjectBody::View(view) => {
                let (columns, more_columns) = hover_columns_from_view(view);
                return HoverInfo::Table {
                    qualified_name: target.qualified_name(),
                    is_view: true,
                    columns,
                    more_columns,
                };
            }
            _ => {}
        }
    }
    let (columns, more_columns) = catalog_object(catalog, target)
        .map(|obj| hover_columns_from_names(&obj.columns))
        .unwrap_or_else(|| (Vec::new(), 0));
    HoverInfo::Table {
        qualified_name: target.qualified_name(),
        is_view,
        columns,
        more_columns,
    }
}

fn column_hover(
    target: &NavigationTarget,
    name: &str,
    entry: Option<&sqlly_lang::sqlmodel::ObjectEntry>,
    catalog: &CompletionCatalog,
) -> HoverColumn {
    let fold = name.to_lowercase();
    if let Some(entry) = entry {
        if let Some(table) = entry.table()
            && let Some(col) = table.column_by_fold(&fold)
        {
            return hover_column_from_def(col, table.is_key_column(&col.name));
        }
        if let ObjectBody::View(view) = &entry.body
            && let Some(col) = view.columns.iter().find(|c| c.name.fold_key() == fold)
        {
            return hover_column_from_def(col, false);
        }
    }
    if let Some(obj) = catalog_object(catalog, target)
        && let Some(col) = obj.columns.iter().find(|c| c.eq_ignore_ascii_case(name))
    {
        return HoverColumn {
            name: col.clone(),
            data_type: String::new(),
            nullable: None,
            is_primary_key: false,
        };
    }
    HoverColumn {
        name: name.to_string(),
        data_type: String::new(),
        nullable: None,
        is_primary_key: false,
    }
}

fn routine_hover(
    target: &NavigationTarget,
    kind: RoutineKind,
    entry: Option<&sqlly_lang::sqlmodel::ObjectEntry>,
    routines: &[RoutineSignature],
    catalog: &CompletionCatalog,
) -> HoverInfo {
    let qualified_name = target.qualified_name();
    if let Some(sig) = match_routine(routines, target.schema(), target.name()) {
        return HoverInfo::Routine {
            qualified_name,
            kind,
            parameters: sig.parameters.clone(),
            returns: entry.and_then(|e| {
                e.routine()
                    .and_then(|r| r.returns.as_ref().map(|t| t.native.clone()))
            }),
        };
    }
    if let Some(routine) = entry.and_then(|e| e.routine()) {
        let parameters = routine
            .params
            .iter()
            .map(|p| HoverParameter {
                name: p.name.spelling().to_string(),
                data_type: p.ty.as_ref().map(|t| t.native.clone()).unwrap_or_default(),
                is_output: matches!(p.mode, ParamMode::Out | ParamMode::InOut),
            })
            .collect();
        return HoverInfo::Routine {
            qualified_name,
            kind,
            parameters,
            returns: routine.returns.as_ref().map(|t| t.native.clone()),
        };
    }
    let parameters = catalog_object(catalog, target)
        .map(|obj| {
            obj.parameters
                .iter()
                .map(|name| HoverParameter {
                    name: name.clone(),
                    data_type: String::new(),
                    is_output: false,
                })
                .collect()
        })
        .unwrap_or_default();
    HoverInfo::Routine {
        qualified_name,
        kind,
        parameters,
        returns: None,
    }
}

fn match_routine<'a>(
    routines: &'a [RoutineSignature],
    schema: &str,
    name: &str,
) -> Option<&'a RoutineSignature> {
    routines
        .iter()
        .find(|r| r.name.eq_ignore_ascii_case(name) && r.schema.eq_ignore_ascii_case(schema))
}

fn catalog_object<'a>(
    catalog: &'a CompletionCatalog,
    target: &NavigationTarget,
) -> Option<&'a CompletionObject> {
    catalog.objects.iter().find(|o| {
        o.name.eq_ignore_ascii_case(target.name())
            && o.schema
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(target.schema()))
    })
}

fn hover_column_from_def(
    col: &sqlly_lang::sqlmodel::ColumnDef,
    is_primary_key: bool,
) -> HoverColumn {
    HoverColumn {
        name: col.name.spelling().to_string(),
        data_type: col
            .ty
            .as_ref()
            .map(|t| t.native.clone())
            .unwrap_or_default(),
        nullable: col.nullable,
        is_primary_key,
    }
}

fn hover_columns_from_table(table: &TableBody) -> (Vec<HoverColumn>, usize) {
    truncate_columns(
        table
            .columns
            .iter()
            .map(|c| hover_column_from_def(c, table.is_key_column(&c.name)))
            .collect(),
    )
}

fn hover_columns_from_view(view: &ViewBody) -> (Vec<HoverColumn>, usize) {
    truncate_columns(
        view.columns
            .iter()
            .map(|c| hover_column_from_def(c, false))
            .collect(),
    )
}

fn hover_columns_from_names(names: &[String]) -> (Vec<HoverColumn>, usize) {
    truncate_columns(
        names
            .iter()
            .map(|name| HoverColumn {
                name: name.clone(),
                data_type: String::new(),
                nullable: None,
                is_primary_key: false,
            })
            .collect(),
    )
}

fn truncate_columns(all: Vec<HoverColumn>) -> (Vec<HoverColumn>, usize) {
    if all.len() <= MAX_HOVER_COLUMNS {
        (all, 0)
    } else {
        let more = all.len() - MAX_HOVER_COLUMNS;
        (all.into_iter().take(MAX_HOVER_COLUMNS).collect(), more)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::ObjectKind as CoreKind;
    use sqlly_lang::sqlmodel::{
        ColumnDef, Fidelity, IncompleteRegions, KeyDef, ObjectBody, ObjectFragment, ObjectKind,
        ParamDef, Provenance, RoutineBody, TableBody, TypeRef,
    };

    fn mref(namespace: Option<&str>, name: &str) -> ObjectRef {
        let dialect = Dialect::SqlServer;
        ObjectRef::new(
            None,
            namespace.map(|n| SqlName::new(n, dialect, false)),
            SqlName::new(name, dialect, false),
        )
    }

    fn col(name: &str, ty: &str, nullable: Option<bool>, ordinal: usize) -> ColumnDef {
        let dialect = Dialect::SqlServer;
        ColumnDef {
            name: SqlName::new(name, dialect, false),
            ty: Some(TypeRef::parse(ty, dialect)),
            nullable,
            default: None,
            identity: false,
            generated: None,
            collation: None,
            ordinal,
        }
    }

    fn table_fragment(
        namespace: &str,
        name: &str,
        columns: Vec<ColumnDef>,
        pk: &[&str],
    ) -> ObjectFragment {
        let dialect = Dialect::SqlServer;
        ObjectFragment {
            reference: mref(Some(namespace), name),
            kind: ObjectKind::Table,
            dialect,
            body: ObjectBody::Table(TableBody {
                primary_key: if pk.is_empty() {
                    None
                } else {
                    Some(KeyDef {
                        name: None,
                        columns: pk
                            .iter()
                            .map(|c| SqlName::new(*c, dialect, false))
                            .collect(),
                    })
                },
                columns,
                ..TableBody::default()
            }),
            provenance: Provenance::Database {
                profile: "test".into(),
            },
            fidelity: Fidelity::Exact,
            references: Vec::new(),
        }
    }

    fn proc_fragment(namespace: &str, name: &str, params: Vec<ParamDef>) -> ObjectFragment {
        let dialect = Dialect::SqlServer;
        ObjectFragment {
            reference: mref(Some(namespace), name),
            kind: ObjectKind::Procedure,
            dialect,
            body: ObjectBody::Routine(RoutineBody {
                params,
                returns: None,
                language: None,
                body_text: None,
            }),
            provenance: Provenance::Database {
                profile: "test".into(),
            },
            fidelity: Fidelity::Exact,
            references: Vec::new(),
        }
    }

    fn snapshot_from(fragments: Vec<ObjectFragment>) -> ModelSnapshot {
        ModelSnapshot::empty()
            .apply(
                1,
                fragments,
                &Default::default(),
                IncompleteRegions::default(),
            )
            .0
    }

    fn catalog_table(schema: &str, name: &str, columns: &[&str]) -> CompletionObject {
        CompletionObject {
            database: None,
            schema: Some(schema.into()),
            name: name.into(),
            kind: CoreKind::Table,
            columns: columns.iter().map(|s| (*s).to_string()).collect(),
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    fn catalog_proc(schema: &str, name: &str, parameters: &[&str]) -> CompletionObject {
        CompletionObject {
            database: None,
            schema: Some(schema.into()),
            name: name.into(),
            kind: CoreKind::StoredProcedure,
            columns: Vec::new(),
            parameters: parameters.iter().map(|s| (*s).to_string()).collect(),
            foreign_keys: Vec::new(),
        }
    }

    fn catalog_with(objects: Vec<CompletionObject>) -> CompletionCatalog {
        CompletionCatalog {
            databases: vec!["appdb".into()],
            schemas: vec!["dbo".into()],
            objects,
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        }
    }

    fn offset_of(sql: &str, needle: &str) -> usize {
        sql.find(needle).expect("needle")
    }

    #[test]
    fn hover_on_table_lists_typed_columns_with_pk_marks() {
        let sql = "SELECT * FROM dbo.Orders";
        let catalog = catalog_with(vec![catalog_table(
            "dbo",
            "Orders",
            &["Id", "CustomerId", "Total"],
        )]);
        let snapshot = snapshot_from(vec![table_fragment(
            "dbo",
            "Orders",
            vec![
                col("Id", "int", Some(false), 0),
                col("CustomerId", "int", Some(true), 1),
                col("Total", "decimal(18,2)", Some(false), 2),
            ],
            &["Id"],
        )]);
        let (info, _) = hover_at(
            sql,
            offset_of(sql, "Orders"),
            &catalog,
            &snapshot,
            &[],
            "dbo",
            Some("appdb"),
        )
        .expect("hover");
        match info {
            HoverInfo::Table {
                qualified_name,
                is_view,
                columns,
                more_columns,
            } => {
                assert_eq!(qualified_name, "dbo.Orders");
                assert!(!is_view);
                assert_eq!(more_columns, 0);
                assert_eq!(
                    columns,
                    vec![
                        HoverColumn {
                            name: "Id".into(),
                            data_type: "int".into(),
                            nullable: Some(false),
                            is_primary_key: true,
                        },
                        HoverColumn {
                            name: "CustomerId".into(),
                            data_type: "int".into(),
                            nullable: Some(true),
                            is_primary_key: false,
                        },
                        HoverColumn {
                            name: "Total".into(),
                            data_type: "decimal(18,2)".into(),
                            nullable: Some(false),
                            is_primary_key: false,
                        },
                    ]
                );
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_table_truncates_after_max_columns() {
        let names: Vec<String> = (0..15).map(|i| format!("C{i}")).collect();
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let sql = "SELECT * FROM dbo.Wide";
        let catalog = catalog_with(vec![catalog_table("dbo", "Wide", &name_refs)]);
        let columns: Vec<ColumnDef> = names
            .iter()
            .enumerate()
            .map(|(i, n)| col(n, "int", Some(true), i))
            .collect();
        let snapshot = snapshot_from(vec![table_fragment("dbo", "Wide", columns, &[])]);
        let (info, _) = hover_at(
            sql,
            offset_of(sql, "Wide"),
            &catalog,
            &snapshot,
            &[],
            "dbo",
            None,
        )
        .expect("hover");
        match info {
            HoverInfo::Table {
                columns,
                more_columns,
                ..
            } => {
                assert_eq!(columns.len(), MAX_HOVER_COLUMNS);
                assert_eq!(more_columns, 3);
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_alias_qualified_column_reports_table_and_type() {
        let sql = "SELECT o.Total FROM dbo.Orders o";
        let catalog = catalog_with(vec![catalog_table(
            "dbo",
            "Orders",
            &["Id", "CustomerId", "Total"],
        )]);
        let snapshot = snapshot_from(vec![table_fragment(
            "dbo",
            "Orders",
            vec![
                col("Id", "int", Some(false), 0),
                col("CustomerId", "int", Some(true), 1),
                col("Total", "decimal(18,2)", Some(true), 2),
            ],
            &["Id"],
        )]);
        let (info, _) = hover_at(
            sql,
            offset_of(sql, "Total"),
            &catalog,
            &snapshot,
            &[],
            "dbo",
            None,
        )
        .expect("hover");
        match info {
            HoverInfo::Column {
                table_qualified_name,
                column,
            } => {
                assert_eq!(table_qualified_name, "dbo.Orders");
                assert_eq!(column.name, "Total");
                assert_eq!(column.data_type, "decimal(18,2)");
            }
            other => panic!("expected column, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_procedure_prefers_live_signature_types() {
        let sql = "EXEC dbo.GetOrder @Id = 1";
        let catalog = catalog_with(vec![catalog_proc("dbo", "GetOrder", &["@Id"])]);
        let snapshot = snapshot_from(vec![proc_fragment(
            "dbo",
            "GetOrder",
            vec![ParamDef {
                name: SqlName::new("@Id", Dialect::SqlServer, false),
                ty: Some(TypeRef::parse("bigint", Dialect::SqlServer)),
                mode: ParamMode::In,
                default: None,
            }],
        )]);
        let live = [RoutineSignature {
            schema: "dbo".into(),
            name: "GetOrder".into(),
            parameters: vec![HoverParameter {
                name: "@Id".into(),
                data_type: "int".into(),
                is_output: false,
            }],
        }];
        let (info, _) = hover_at(
            sql,
            offset_of(sql, "GetOrder"),
            &catalog,
            &snapshot,
            &live,
            "dbo",
            None,
        )
        .expect("hover");
        match info {
            HoverInfo::Routine { parameters, .. } => {
                assert_eq!(parameters.len(), 1);
                assert_eq!(parameters[0].data_type, "int");
            }
            other => panic!("expected routine, got {other:?}"),
        }
    }

    #[test]
    fn hover_returns_none_for_keywords_and_unknown_names() {
        let catalog = catalog_with(vec![]);
        let snapshot = ModelSnapshot::empty();
        let sql = "SELECT * FROM Nope";
        assert!(
            hover_at(
                sql,
                offset_of(sql, "SELECT"),
                &catalog,
                &snapshot,
                &[],
                "dbo",
                None
            )
            .is_none()
        );
        assert!(
            hover_at(
                sql,
                offset_of(sql, "Nope"),
                &catalog,
                &snapshot,
                &[],
                "dbo",
                None
            )
            .is_none()
        );
    }

    #[test]
    fn hover_falls_back_to_catalog_names_when_snapshot_lacks_object() {
        let sql = "SELECT * FROM dbo.Orders";
        let catalog = catalog_with(vec![catalog_table("dbo", "Orders", &["Id", "CustomerId"])]);
        let snapshot = ModelSnapshot::empty();
        let (info, _) = hover_at(
            sql,
            offset_of(sql, "Orders"),
            &catalog,
            &snapshot,
            &[],
            "dbo",
            None,
        )
        .expect("hover");
        match info {
            HoverInfo::Table { columns, .. } => {
                assert_eq!(columns.len(), 2);
                assert!(columns.iter().all(|c| c.data_type.is_empty()));
                assert!(columns.iter().all(|c| c.nullable.is_none()));
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn hover_span_covers_bracketed_identifier() {
        let sql = "SELECT * FROM [Orders]";
        let catalog = catalog_with(vec![catalog_table("dbo", "Orders", &["Id"])]);
        let snapshot = snapshot_from(vec![table_fragment(
            "dbo",
            "Orders",
            vec![col("Id", "int", Some(false), 0)],
            &["Id"],
        )]);
        let (_, span) = hover_at(
            sql,
            offset_of(sql, "Orders"),
            &catalog,
            &snapshot,
            &[],
            "dbo",
            None,
        )
        .expect("hover");
        assert_eq!(&sql[span.0..span.1], "[Orders]");
    }
}

#[cfg(test)]
mod builtin_hover_tests {
    use super::*;

    fn at(dialect: Dialect, sql: &str) -> Option<HoverInfo> {
        let offset = sql.find('|').expect("cursor mark");
        let text = sql.replacen('|', "", 1);
        builtin_hover_at(dialect, &text, offset).map(|(info, _)| info)
    }

    #[test]
    fn a_called_builtin_shows_its_function_card() {
        let Some(HoverInfo::Builtin(f)) = at(Dialect::SqlServer, "SELECT DATE|ADD(day, 1, x)")
        else {
            panic!("expected a builtin card");
        };
        assert_eq!(f.name, "DATEADD");
        assert!(!f.summary.is_empty());
        let Some(HoverInfo::Builtin(f)) = at(Dialect::Postgres, "SELECT no|w()") else {
            panic!("expected a builtin card");
        };
        assert_eq!(f.name, "now");
    }

    #[test]
    fn system_names_have_cards_in_their_own_dialect() {
        assert!(matches!(
            at(Dialect::SqlServer, "SELECT @@ROW|COUNT"),
            Some(HoverInfo::Builtin(_))
        ));
        assert!(matches!(
            at(Dialect::MySql, "SELECT @@sql_mo|de"),
            Some(HoverInfo::SystemVariable(_))
        ));
        assert_eq!(at(Dialect::Postgres, "SELECT @@sql_mo|de"), None);
    }

    #[test]
    fn keywords_have_no_card() {
        assert_eq!(at(Dialect::SqlServer, "SEL|ECT 1"), None);
        assert_eq!(at(Dialect::SqlServer, "SELECT a FROM t ORDER B|Y a"), None);
        // A keyword that is also a type (`CREATE TABLE`) stays quiet too.
        assert_eq!(at(Dialect::SqlServer, "CREATE TAB|LE t (a int)"), None);
        // …but the same spelling called as a function gets its card.
        assert!(matches!(
            at(Dialect::SqlServer, "SELECT LE|FT(name, 3)"),
            Some(HoverInfo::Builtin(_))
        ));
    }

    #[test]
    fn constants_types_and_slot_words_have_cards() {
        assert!(matches!(
            at(Dialect::Postgres, "WHERE a IS NU|LL"),
            Some(HoverInfo::Constant { slot: None, .. })
        ));
        let Some(HoverInfo::Constant {
            slot: Some(slot), ..
        }) = at(Dialect::SqlServer, "SELECT DATEDIFF(min|ute, a, b)")
        else {
            panic!("expected a slot constant");
        };
        assert!(slot.contains("DATEDIFF"), "{slot}");
        assert!(matches!(
            at(Dialect::SqlServer, "DECLARE @x NVAR|CHAR(10)"),
            Some(HoverInfo::DataType(_))
        ));
    }

    #[test]
    fn names_are_never_vocabulary() {
        // Delimited identifiers and qualified parts are user objects.
        assert_eq!(at(Dialect::SqlServer, "SELECT [cou|nt] FROM t"), None);
        assert_eq!(at(Dialect::SqlServer, "SELECT t.cou|nt FROM t"), None);
        assert_eq!(at(Dialect::SqlServer, "SELECT dbo.L|EN(x)"), None);
        // An unknown word has no card; a column sharing a function's name
        // without a call does not pretend to be the function unless nothing
        // else claims it.
        assert_eq!(at(Dialect::SqlServer, "SELECT fo|obar FROM t"), None);
    }
}
