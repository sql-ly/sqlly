//! Click-to-definition symbol resolution ("DDL navigation").
//!
//! Given a byte offset in a SQL buffer (from the editor's mouse hit-test) and
//! the local completion catalog, [`resolve_symbol_at`] determines which
//! database object the clicked identifier refers to — entirely locally, with
//! no database round trip (per AGENTS.md the click hot path must stay
//! performant). The caller then fetches the object's DDL asynchronously.
//!
//! Resolution handles dotted/bracketed/double-quoted identifiers
//! (`Users`, `dbo.Users`, `[dbo].[Users]`, `"dbo"."Users"`, `db.dbo.Users`),
//! FROM/JOIN alias scopes (`u.Name` → `dbo.Users`), `EXEC … @Param` call
//! context, and refuses to guess when a bare column is ambiguous across the
//! tables in scope. Clicks inside comments and string literals never resolve
//! (the scanner skips them, so no token covers those offsets).

use sqlly_core::ObjectKind;

use crate::intellisense::complete::{
    detect_clause_context, looks_like_keyword, query_table_refs, scan,
};
use crate::intellisense::types::{CompletionCatalog, CompletionObject};

/// What kind of routine a [`NavigationTarget::Routine`] refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineKind {
    Procedure,
    Function,
}

/// The specific symbol the user clicked, carried alongside the resolved
/// parent object so presentation can label the destination ("column `Name`
/// of table `dbo.Users`").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickedRole {
    Table,
    View,
    Column { name: String },
    Index { name: String },
    Type,
    Procedure,
    ProcedureParameter { name: String },
    Function,
}

/// A resolved navigation destination. Columns and indexes resolve to their
/// parent table; procedure parameters resolve to the owning procedure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationTarget {
    Table {
        catalog: Option<String>,
        schema: String,
        name: String,
        clicked: ClickedRole,
    },
    Type {
        catalog: Option<String>,
        schema: String,
        name: String,
    },
    Routine {
        catalog: Option<String>,
        schema: String,
        name: String,
        kind: RoutineKind,
        clicked: ClickedRole,
    },
}

impl NavigationTarget {
    pub fn schema(&self) -> &str {
        match self {
            NavigationTarget::Table { schema, .. }
            | NavigationTarget::Type { schema, .. }
            | NavigationTarget::Routine { schema, .. } => schema,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            NavigationTarget::Table { name, .. }
            | NavigationTarget::Type { name, .. }
            | NavigationTarget::Routine { name, .. } => name,
        }
    }

    pub fn catalog(&self) -> Option<&str> {
        match self {
            NavigationTarget::Table { catalog, .. }
            | NavigationTarget::Type { catalog, .. }
            | NavigationTarget::Routine { catalog, .. } => catalog.as_deref(),
        }
    }

    /// `schema.name` — the display / title form of the target.
    pub fn qualified_name(&self) -> String {
        format!("{}.{}", self.schema(), self.name())
    }
}

/// Result of a click resolution. `Ambiguous` is distinct from `NotFound` so
/// the app can show "column `id` is ambiguous" instead of a generic no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolResolution {
    Resolved(NavigationTarget),
    /// The clicked name matched more than one object and no scope information
    /// disambiguated it. Guessing is worse than a no-op.
    Ambiguous {
        name: String,
        candidates: Vec<String>,
    },
    NotFound,
}

/// Resolve the database object under `offset` in `sql` against the local
/// completion catalog. Pure and synchronous — no I/O.
///
/// `default_schema` (usually `dbo`) breaks ties when a bare object name
/// exists in several schemas. `default_database` is the connection's active
/// database, used to match 3-part names against catalog objects that carry
/// no explicit database.
pub fn resolve_symbol_at(
    sql: &str,
    offset: usize,
    catalog: &CompletionCatalog,
    default_schema: &str,
    default_database: Option<&str>,
) -> SymbolResolution {
    if offset > sql.len() {
        return SymbolResolution::NotFound;
    }
    let tokens = scan::scan_tokens(sql);
    let Some(clicked_idx) = token_at_offset(&tokens, offset) else {
        return SymbolResolution::NotFound;
    };
    let clicked_text = tokens[clicked_idx].text.as_str().to_string();

    // `@param` → resolve the owning procedure from the surrounding EXEC call.
    if clicked_text.starts_with('@') {
        if clicked_text.starts_with("@@") {
            return SymbolResolution::NotFound; // system variable
        }
        return resolve_exec_parameter(
            &tokens,
            clicked_idx,
            &clicked_text,
            catalog,
            default_schema,
            default_database,
        );
    }

    let (parts, clicked_part) = dotted_chain(&tokens, clicked_idx);

    if parts.len() == 1 {
        return resolve_bare_word(
            sql,
            offset,
            &clicked_text,
            catalog,
            default_schema,
            default_database,
        );
    }

    // Alias-qualified column: `u.Name` where `u` aliases a FROM/JOIN table.
    if parts.len() == 2 {
        let alias = &parts[0];
        if let Some(table_name) = alias_target(sql, offset, alias)
            && let Lookup::One(object) = lookup_named(catalog, &table_name, default_schema)
        {
            return if clicked_part == 0 {
                object_target(object, None, default_schema)
            } else {
                column_target(object, &parts[1], default_schema)
            };
        }
    }

    // Full chain as an object reference: `dbo.Users`, `db.dbo.Users`.
    if parts.len() <= 3
        && let Lookup::One(object) = lookup_parts(catalog, &parts, default_schema, default_database)
    {
        return object_target(object, None, default_schema);
    }

    // Chain minus the last part as an object, last part as its column:
    // `Users.Email`, `dbo.Users.Email`, `db.dbo.Users.Email`.
    if parts.len() >= 2 && parts.len() <= 4 {
        let object_parts = &parts[..parts.len() - 1];
        let lookup = if object_parts.len() == 1 {
            lookup_named(catalog, &object_parts[0], default_schema)
        } else {
            lookup_parts(catalog, object_parts, default_schema, default_database)
        };
        if let Lookup::One(object) = lookup
            && matches!(object.kind, ObjectKind::Table | ObjectKind::View)
        {
            return if clicked_part == parts.len() - 1 {
                column_target(object, &parts[parts.len() - 1], default_schema)
            } else {
                object_target(object, None, default_schema)
            };
        }
    }

    SymbolResolution::NotFound
}

/// Source span (including bracket/quote delimiters) of the navigable token
/// under `offset`, or `None` when nothing under the cursor would navigate.
/// Drives the editor's modifier-hover "hyperlink" affordance, so it applies
/// exactly the same resolution rules as [`resolve_symbol_at`] — only spans
/// that would actually navigate light up.
pub fn navigable_span_at(
    sql: &str,
    offset: usize,
    catalog: &CompletionCatalog,
    default_schema: &str,
    default_database: Option<&str>,
) -> Option<(usize, usize)> {
    if !matches!(
        resolve_symbol_at(sql, offset, catalog, default_schema, default_database),
        SymbolResolution::Resolved(_)
    ) {
        return None;
    }
    let tokens = scan::scan_tokens(sql);
    let clicked = token_at_offset(&tokens, offset)?;
    Some((tokens[clicked].byte_offset, tokens[clicked].end_byte))
}

// ---------------------------------------------------------------------------
// Token helpers
// ---------------------------------------------------------------------------

/// Index of the Word token whose source span contains `offset`. A click on
/// the trailing edge of a word (offset == end) still counts, matching how
/// the editor's hit-test snaps the caret between characters.
pub(crate) fn token_at_offset(tokens: &[scan::ScanToken<'_>], offset: usize) -> Option<usize> {
    let mut trailing: Option<usize> = None;
    for (i, tok) in tokens.iter().enumerate() {
        if tok.byte_offset > offset {
            break;
        }
        if tok.kind != scan::ScanTokenKind::Word {
            continue;
        }
        if offset < tok.end_byte {
            return Some(i);
        }
        if offset == tok.end_byte {
            trailing = Some(i);
        }
    }
    trailing
}

/// Expand the dotted identifier chain containing `idx` (`db . schema . name`
/// as consecutive Word/Dot tokens). Returns the chain's part texts and the
/// index of the clicked part within it. Keywords never merge across dots,
/// matching `depth_words`' behavior.
fn dotted_chain(tokens: &[scan::ScanToken<'_>], idx: usize) -> (Vec<String>, usize) {
    let is_chain_word = |i: usize| {
        tokens.get(i).is_some_and(|t| {
            t.kind == scan::ScanTokenKind::Word
                && !t.text.as_str().starts_with('@')
                && !looks_like_keyword(t.text.as_str())
        })
    };
    let is_dot = |i: usize| {
        tokens
            .get(i)
            .is_some_and(|t| t.kind == scan::ScanTokenKind::Dot)
    };

    let mut start = idx;
    while start >= 2 && is_dot(start - 1) && is_chain_word(start - 2) {
        start -= 2;
    }
    let mut end = idx;
    while is_dot(end + 1) && is_chain_word(end + 2) {
        end += 2;
    }

    let parts: Vec<String> = (start..=end)
        .step_by(2)
        .map(|i| tokens[i].text.as_str().to_string())
        .collect();
    ((parts), (idx - start) / 2)
}

// ---------------------------------------------------------------------------
// Catalog lookup
// ---------------------------------------------------------------------------

enum Lookup<'a> {
    One(&'a CompletionObject),
    Many(Vec<&'a CompletionObject>),
    None,
}

fn navigable(object: &CompletionObject) -> bool {
    matches!(
        object.kind,
        ObjectKind::Table
            | ObjectKind::View
            | ObjectKind::StoredProcedure
            | ObjectKind::ScalarFunction
            | ObjectKind::TableFunction
            | ObjectKind::AggregateFunction
            | ObjectKind::Type
    )
}

/// Look up a possibly schema-qualified name (`Users` / `dbo.Users`). Bare
/// names that exist in several schemas prefer `default_schema`.
fn lookup_named<'a>(
    catalog: &'a CompletionCatalog,
    name: &str,
    default_schema: &str,
) -> Lookup<'a> {
    let name = name.trim_matches(|ch| ch == '[' || ch == ']');
    match name.rsplit_once('.') {
        Some((schema, bare)) => lookup_parts(
            catalog,
            &[schema.to_string(), bare.to_string()],
            default_schema,
            None,
        ),
        None => lookup_parts(catalog, &[name.to_string()], default_schema, None),
    }
}

/// Look up a 1/2/3-part reference against catalog objects.
fn lookup_parts<'a>(
    catalog: &'a CompletionCatalog,
    parts: &[String],
    default_schema: &str,
    default_database: Option<&str>,
) -> Lookup<'a> {
    let matches: Vec<&CompletionObject> = catalog
        .objects
        .iter()
        .filter(|o| navigable(o))
        .filter(|o| match parts {
            [name] => o.name.eq_ignore_ascii_case(name),
            [schema, name] => {
                o.name.eq_ignore_ascii_case(name)
                    && o.schema
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(schema))
            }
            [db, schema, name] => {
                o.name.eq_ignore_ascii_case(name)
                    && o.schema
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(schema))
                    && match o.database.as_deref() {
                        Some(d) => d.eq_ignore_ascii_case(db),
                        // Objects without an explicit database belong to the
                        // active one; only match if the 3-part ref names it.
                        None => default_database.is_some_and(|d| d.eq_ignore_ascii_case(db)),
                    }
            }
            _ => false,
        })
        .collect();

    match matches.len() {
        0 => Lookup::None,
        1 => Lookup::One(matches[0]),
        _ => {
            // Bare-name collision across schemas: prefer the default schema.
            let preferred: Vec<&CompletionObject> = matches
                .iter()
                .copied()
                .filter(|o| {
                    o.schema
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(default_schema))
                })
                .collect();
            if preferred.len() == 1 {
                Lookup::One(preferred[0])
            } else {
                Lookup::Many(matches)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Target construction
// ---------------------------------------------------------------------------

fn object_schema(object: &CompletionObject, default_schema: &str) -> String {
    object
        .schema
        .clone()
        .unwrap_or_else(|| default_schema.to_string())
}

/// Build the navigation target for a directly-clicked object. `column` is
/// `Some` when the click was on a column of that object.
fn object_target(
    object: &CompletionObject,
    column: Option<&str>,
    default_schema: &str,
) -> SymbolResolution {
    let schema = object_schema(object, default_schema);
    let catalog = object.database.clone();
    let name = object.name.clone();
    let target = match object.kind {
        ObjectKind::Table | ObjectKind::View => {
            let clicked = match column {
                Some(col) => ClickedRole::Column {
                    name: col.to_string(),
                },
                None if object.kind == ObjectKind::View => ClickedRole::View,
                None => ClickedRole::Table,
            };
            NavigationTarget::Table {
                catalog,
                schema,
                name,
                clicked,
            }
        }
        ObjectKind::StoredProcedure => NavigationTarget::Routine {
            catalog,
            schema,
            name,
            kind: RoutineKind::Procedure,
            clicked: ClickedRole::Procedure,
        },
        ObjectKind::ScalarFunction | ObjectKind::TableFunction | ObjectKind::AggregateFunction => {
            NavigationTarget::Routine {
                catalog,
                schema,
                name,
                kind: RoutineKind::Function,
                clicked: ClickedRole::Function,
            }
        }
        ObjectKind::Type => NavigationTarget::Type {
            catalog,
            schema,
            name,
        },
        // Indexes are not currently present in the completion snapshot, and
        // the catalog carries no parent-table link to resolve one through.
        _ => return SymbolResolution::NotFound,
    };
    SymbolResolution::Resolved(target)
}

/// Column click on a known table/view. When the object's column list is
/// loaded and the name isn't in it, refuse — navigating to a table the
/// column doesn't belong to would be misleading. An empty column list means
/// columns simply haven't been loaded yet; navigate optimistically.
fn column_target(
    object: &CompletionObject,
    column: &str,
    default_schema: &str,
) -> SymbolResolution {
    if !object.columns.is_empty()
        && !object
            .columns
            .iter()
            .any(|c| c.eq_ignore_ascii_case(column))
    {
        return SymbolResolution::NotFound;
    }
    object_target(object, Some(column), default_schema)
}

// ---------------------------------------------------------------------------
// Bare-word resolution
// ---------------------------------------------------------------------------

/// Resolve a single unqualified word. In column-expecting clauses (SELECT
/// list, WHERE, …) in-scope column resolution is tried before object-name
/// lookup; in table-expecting clauses (FROM, JOIN) the order flips.
fn resolve_bare_word(
    sql: &str,
    offset: usize,
    word: &str,
    catalog: &CompletionCatalog,
    default_schema: &str,
    default_database: Option<&str>,
) -> SymbolResolution {
    let context = detect_clause_context(sql, offset);
    let column_first = context.is_column_expecting();

    let as_object = || match lookup_parts(
        catalog,
        &[word.to_string()],
        default_schema,
        default_database,
    ) {
        Lookup::One(object) => object_target(object, None, default_schema),
        Lookup::Many(objects) => SymbolResolution::Ambiguous {
            name: word.to_string(),
            candidates: objects
                .iter()
                .map(|o| format!("{}.{}", object_schema(o, default_schema), o.name))
                .collect(),
        },
        Lookup::None => SymbolResolution::NotFound,
    };

    let as_scoped_column = || resolve_scoped_column(sql, offset, word, catalog, default_schema);

    let (first, second): (&dyn Fn() -> SymbolResolution, &dyn Fn() -> SymbolResolution) =
        if column_first {
            (&as_scoped_column, &as_object)
        } else {
            (&as_object, &as_scoped_column)
        };

    match first() {
        SymbolResolution::NotFound => second(),
        resolved => resolved,
    }
}

/// Find which in-scope FROM/JOIN table owns `word` as a column. Exactly one
/// owner resolves; several is ambiguous; none is not-found.
fn resolve_scoped_column(
    sql: &str,
    offset: usize,
    word: &str,
    catalog: &CompletionCatalog,
    default_schema: &str,
) -> SymbolResolution {
    let refs = query_table_refs(sql, offset);
    let mut owners: Vec<&CompletionObject> = Vec::new();
    for table_ref in &refs {
        if let Lookup::One(object) = lookup_named(catalog, &table_ref.name, default_schema)
            && object.columns.iter().any(|c| c.eq_ignore_ascii_case(word))
            && !owners.iter().any(|o| std::ptr::eq(*o, object))
        {
            owners.push(object);
        }
    }
    match owners.len() {
        0 => SymbolResolution::NotFound,
        1 => object_target(owners[0], Some(word), default_schema),
        _ => SymbolResolution::Ambiguous {
            name: word.to_string(),
            candidates: owners
                .iter()
                .map(|o| format!("{}.{}", object_schema(o, default_schema), o.name))
                .collect(),
        },
    }
}

/// Alias → table-name lookup from the FROM/JOIN clauses in scope at `offset`.
fn alias_target(sql: &str, offset: usize, alias: &str) -> Option<String> {
    crate::intellisense::query_aliases(sql, offset)
        .into_iter()
        .find(|(a, _)| a.eq_ignore_ascii_case(alias))
        .map(|(_, table)| table)
}

// ---------------------------------------------------------------------------
// EXEC parameter resolution
// ---------------------------------------------------------------------------

/// Words that start a new statement — walking back past one of these means
/// the clicked `@param` is not inside an EXEC call's argument list.
fn is_statement_boundary(word: &str) -> bool {
    word.eq_ignore_ascii_case("SELECT")
        || word.eq_ignore_ascii_case("INSERT")
        || word.eq_ignore_ascii_case("UPDATE")
        || word.eq_ignore_ascii_case("DELETE")
        || word.eq_ignore_ascii_case("DECLARE")
        || word.eq_ignore_ascii_case("SET")
        || word.eq_ignore_ascii_case("WITH")
        || word.eq_ignore_ascii_case("CREATE")
        || word.eq_ignore_ascii_case("ALTER")
        || word.eq_ignore_ascii_case("DROP")
        || word.eq_ignore_ascii_case("WHILE")
        || word.eq_ignore_ascii_case("IF")
        || word.eq_ignore_ascii_case("BEGIN")
        || word.eq_ignore_ascii_case("RETURN")
        || word.eq_ignore_ascii_case("GO")
        || word.eq_ignore_ascii_case("MERGE")
        || word.eq_ignore_ascii_case("TRUNCATE")
        || word.eq_ignore_ascii_case("PRINT")
        || word.eq_ignore_ascii_case("USE")
}

/// `EXEC dbo.GetUser @CustomerId = 1` — clicking `@CustomerId` resolves to
/// `dbo.GetUser` via the surrounding call, never by parameter name alone
/// (parameter names are reused across procedures).
fn resolve_exec_parameter(
    tokens: &[scan::ScanToken<'_>],
    clicked_idx: usize,
    param: &str,
    catalog: &CompletionCatalog,
    default_schema: &str,
    default_database: Option<&str>,
) -> SymbolResolution {
    // Walk backward to the EXEC/EXECUTE keyword of the enclosing call.
    let mut exec_idx: Option<usize> = None;
    for i in (0..clicked_idx).rev() {
        let tok = &tokens[i];
        match tok.kind {
            scan::ScanTokenKind::Semicolon => break,
            scan::ScanTokenKind::Word => {
                let text = tok.text.as_str();
                if text.eq_ignore_ascii_case("EXEC") || text.eq_ignore_ascii_case("EXECUTE") {
                    exec_idx = Some(i);
                    break;
                }
                if is_statement_boundary(text) {
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(exec_idx) = exec_idx else {
        return SymbolResolution::NotFound;
    };

    // The procedure name is the dotted chain after EXEC, skipping an
    // optional `@return_value =` capture.
    let mut name_idx = exec_idx + 1;
    if tokens
        .get(name_idx)
        .is_some_and(|t| t.kind == scan::ScanTokenKind::Word && t.text.as_str().starts_with('@'))
        && tokens
            .get(name_idx + 1)
            .is_some_and(|t| t.kind == scan::ScanTokenKind::Other && t.text.as_str() == "=")
    {
        name_idx += 2;
    }
    if !tokens
        .get(name_idx)
        .is_some_and(|t| t.kind == scan::ScanTokenKind::Word && !t.text.as_str().starts_with('@'))
    {
        return SymbolResolution::NotFound;
    }

    let (parts, _) = dotted_chain(tokens, name_idx);
    if parts.is_empty() || parts.len() > 3 {
        return SymbolResolution::NotFound;
    }
    match lookup_parts(catalog, &parts, default_schema, default_database) {
        Lookup::One(object)
            if matches!(
                object.kind,
                ObjectKind::StoredProcedure
                    | ObjectKind::ScalarFunction
                    | ObjectKind::TableFunction
                    | ObjectKind::AggregateFunction
            ) =>
        {
            let kind = if object.kind == ObjectKind::StoredProcedure {
                RoutineKind::Procedure
            } else {
                RoutineKind::Function
            };
            SymbolResolution::Resolved(NavigationTarget::Routine {
                catalog: object.database.clone(),
                schema: object_schema(object, default_schema),
                name: object.name.clone(),
                kind,
                clicked: ClickedRole::ProcedureParameter {
                    name: param.to_string(),
                },
            })
        }
        _ => SymbolResolution::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intellisense::CompletionObject;

    fn obj(
        schema: &str,
        name: &str,
        kind: ObjectKind,
        columns: &[&str],
        parameters: &[&str],
    ) -> CompletionObject {
        CompletionObject {
            database: None,
            schema: Some(schema.to_string()),
            name: name.to_string(),
            kind,
            columns: columns.iter().map(|s| s.to_string()).collect(),
            parameters: parameters.iter().map(|s| s.to_string()).collect(),
            foreign_keys: Vec::new(),
        }
    }

    fn test_catalog() -> CompletionCatalog {
        CompletionCatalog {
            databases: vec!["appdb".into()],
            schemas: vec!["dbo".into(), "sales".into()],
            objects: vec![
                obj(
                    "dbo",
                    "Users",
                    ObjectKind::Table,
                    &["UserId", "Name", "Email"],
                    &[],
                ),
                obj(
                    "dbo",
                    "Orders",
                    ObjectKind::Table,
                    &["OrderId", "UserId", "Total"],
                    &[],
                ),
                obj(
                    "sales",
                    "Orders",
                    ObjectKind::Table,
                    &["OrderId", "Region"],
                    &[],
                ),
                obj(
                    "dbo",
                    "ActiveUsers",
                    ObjectKind::View,
                    &["UserId", "Name"],
                    &[],
                ),
                obj(
                    "dbo",
                    "GetUser",
                    ObjectKind::StoredProcedure,
                    &[],
                    &["@UserId"],
                ),
                obj("dbo", "udf_total", ObjectKind::ScalarFunction, &[], &[]),
                obj("dbo", "PhoneNumber", ObjectKind::Type, &[], &[]),
                obj("dbo", "LazyTable", ObjectKind::Table, &[], &[]),
            ],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        }
    }

    fn resolve(sql: &str, offset: usize) -> SymbolResolution {
        resolve_symbol_at(sql, offset, &test_catalog(), "dbo", Some("appdb"))
    }

    /// Byte offset of the middle of the first occurrence of `needle`.
    fn mid(sql: &str, needle: &str) -> usize {
        sql.find(needle).expect("needle in sql") + needle.len() / 2
    }

    fn expect_table(res: SymbolResolution, schema: &str, name: &str, clicked: ClickedRole) {
        match res {
            SymbolResolution::Resolved(NavigationTarget::Table {
                schema: s,
                name: n,
                clicked: c,
                ..
            }) => {
                assert_eq!((s.as_str(), n.as_str()), (schema, name));
                assert_eq!(c, clicked);
            }
            other => panic!("expected table target {schema}.{name}, got {other:?}"),
        }
    }

    #[test]
    fn table_click_in_from() {
        let sql = "SELECT * FROM dbo.Users WHERE 1=1";
        expect_table(
            resolve(sql, mid(sql, "Users")),
            "dbo",
            "Users",
            ClickedRole::Table,
        );
    }

    #[test]
    fn schema_part_click_resolves_object() {
        let sql = "SELECT * FROM dbo.Users";
        expect_table(
            resolve(sql, mid(sql, "dbo")),
            "dbo",
            "Users",
            ClickedRole::Table,
        );
    }

    #[test]
    fn view_click() {
        let sql = "SELECT * FROM ActiveUsers";
        expect_table(
            resolve(sql, mid(sql, "ActiveUsers")),
            "dbo",
            "ActiveUsers",
            ClickedRole::View,
        );
    }

    #[test]
    fn alias_column_resolves_to_table() {
        let sql = "SELECT u.Name FROM dbo.Users u";
        expect_table(
            resolve(sql, mid(sql, "Name")),
            "dbo",
            "Users",
            ClickedRole::Column {
                name: "Name".into(),
            },
        );
    }

    #[test]
    fn alias_itself_resolves_to_table() {
        let sql = "SELECT u.Name FROM dbo.Users u";
        expect_table(
            resolve(sql, mid(sql, "u.")),
            "dbo",
            "Users",
            ClickedRole::Table,
        );
    }

    #[test]
    fn table_qualified_column() {
        let sql = "SELECT Users.Email FROM Users";
        expect_table(
            resolve(sql, mid(sql, "Email")),
            "dbo",
            "Users",
            ClickedRole::Column {
                name: "Email".into(),
            },
        );
    }

    #[test]
    fn ambiguous_bare_column_returns_ambiguous() {
        let sql = "SELECT UserId FROM dbo.Users u JOIN dbo.Orders o ON u.UserId = o.UserId";
        match resolve(sql, mid(sql, "UserId")) {
            SymbolResolution::Ambiguous { name, candidates } => {
                assert_eq!(name, "UserId");
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn unique_bare_column_resolves() {
        let sql = "SELECT Email FROM dbo.Users u JOIN dbo.Orders o ON u.UserId = o.UserId";
        expect_table(
            resolve(sql, mid(sql, "Email")),
            "dbo",
            "Users",
            ClickedRole::Column {
                name: "Email".into(),
            },
        );
    }

    #[test]
    fn exec_parameter_resolves_owning_procedure() {
        let sql = "EXEC dbo.GetUser @UserId = 5";
        match resolve(sql, mid(sql, "@UserId")) {
            SymbolResolution::Resolved(NavigationTarget::Routine {
                schema,
                name,
                kind,
                clicked,
                ..
            }) => {
                assert_eq!((schema.as_str(), name.as_str()), ("dbo", "GetUser"));
                assert_eq!(kind, RoutineKind::Procedure);
                assert_eq!(
                    clicked,
                    ClickedRole::ProcedureParameter {
                        name: "@UserId".into()
                    }
                );
            }
            other => panic!("expected routine target, got {other:?}"),
        }
    }

    #[test]
    fn exec_with_return_capture_resolves_parameter() {
        let sql = "DECLARE @rc int; EXEC @rc = dbo.GetUser @UserId = 5";
        match resolve(sql, mid(sql, "@UserId")) {
            SymbolResolution::Resolved(NavigationTarget::Routine { name, .. }) => {
                assert_eq!(name, "GetUser");
            }
            other => panic!("expected routine target, got {other:?}"),
        }
    }

    #[test]
    fn procedure_name_click_in_exec() {
        let sql = "EXEC dbo.GetUser @UserId = 5";
        match resolve(sql, mid(sql, "GetUser")) {
            SymbolResolution::Resolved(NavigationTarget::Routine {
                name,
                kind,
                clicked,
                ..
            }) => {
                assert_eq!(name, "GetUser");
                assert_eq!(kind, RoutineKind::Procedure);
                assert_eq!(clicked, ClickedRole::Procedure);
            }
            other => panic!("expected routine target, got {other:?}"),
        }
    }

    #[test]
    fn declared_variable_without_exec_context_is_not_found() {
        let sql = "DECLARE @UserId int; SELECT @UserId";
        assert_eq!(
            resolve(sql, mid(sql, "@UserId")),
            SymbolResolution::NotFound
        );
    }

    #[test]
    fn function_call_resolves() {
        let sql = "SELECT dbo.udf_total(OrderId) FROM Orders";
        match resolve(sql, mid(sql, "udf_total")) {
            SymbolResolution::Resolved(NavigationTarget::Routine { name, kind, .. }) => {
                assert_eq!(name, "udf_total");
                assert_eq!(kind, RoutineKind::Function);
            }
            other => panic!("expected function target, got {other:?}"),
        }
    }

    #[test]
    fn custom_type_resolves() {
        let sql = "DECLARE @p dbo.PhoneNumber";
        match resolve(sql, mid(sql, "PhoneNumber")) {
            SymbolResolution::Resolved(NavigationTarget::Type { schema, name, .. }) => {
                assert_eq!((schema.as_str(), name.as_str()), ("dbo", "PhoneNumber"));
            }
            other => panic!("expected type target, got {other:?}"),
        }
    }

    #[test]
    fn bracketed_names_resolve() {
        let sql = "SELECT * FROM [dbo].[Users]";
        expect_table(
            resolve(sql, mid(sql, "[Users]")),
            "dbo",
            "Users",
            ClickedRole::Table,
        );
    }

    #[test]
    fn double_quoted_names_resolve() {
        let sql = "SELECT * FROM \"dbo\".\"Users\"";
        expect_table(
            resolve(sql, mid(sql, "\"Users\"")),
            "dbo",
            "Users",
            ClickedRole::Table,
        );
    }

    #[test]
    fn three_part_name_resolves_with_default_database() {
        let sql = "SELECT * FROM appdb.dbo.Users";
        expect_table(
            resolve(sql, mid(sql, "Users")),
            "dbo",
            "Users",
            ClickedRole::Table,
        );
    }

    #[test]
    fn three_part_name_with_other_database_is_not_found() {
        let sql = "SELECT * FROM otherdb.dbo.Users";
        assert_eq!(resolve(sql, mid(sql, "Users")), SymbolResolution::NotFound);
    }

    #[test]
    fn comment_click_is_not_found() {
        let sql = "-- dbo.Users lives here\nSELECT 1";
        assert_eq!(resolve(sql, mid(sql, "Users")), SymbolResolution::NotFound);
    }

    #[test]
    fn string_literal_click_is_not_found() {
        let sql = "SELECT 'dbo.Users'";
        assert_eq!(resolve(sql, mid(sql, "Users")), SymbolResolution::NotFound);
    }

    #[test]
    fn keyword_click_is_not_found() {
        let sql = "SELECT * FROM dbo.Users";
        assert_eq!(resolve(sql, mid(sql, "SELECT")), SymbolResolution::NotFound);
    }

    #[test]
    fn bare_table_name_resolves() {
        let sql = "SELECT * FROM Users";
        expect_table(
            resolve(sql, mid(sql, "Users")),
            "dbo",
            "Users",
            ClickedRole::Table,
        );
    }

    #[test]
    fn bare_name_collision_prefers_default_schema() {
        let sql = "SELECT * FROM Orders";
        expect_table(
            resolve(sql, mid(sql, "Orders")),
            "dbo",
            "Orders",
            ClickedRole::Table,
        );
    }

    #[test]
    fn unknown_column_on_loaded_table_is_not_found() {
        let sql = "SELECT u.Bogus FROM dbo.Users u";
        assert_eq!(resolve(sql, mid(sql, "Bogus")), SymbolResolution::NotFound);
    }

    #[test]
    fn unloaded_columns_navigate_optimistically() {
        let sql = "SELECT l.Anything FROM dbo.LazyTable l";
        expect_table(
            resolve(sql, mid(sql, "Anything")),
            "dbo",
            "LazyTable",
            ClickedRole::Column {
                name: "Anything".into(),
            },
        );
    }

    #[test]
    fn whitespace_click_is_not_found() {
        let sql = "SELECT *  FROM dbo.Users";
        let offset = sql.find("*  ").unwrap() + 2;
        assert_eq!(resolve(sql, offset), SymbolResolution::NotFound);
    }

    #[test]
    fn offset_past_end_is_not_found() {
        assert_eq!(resolve("SELECT 1", 999), SymbolResolution::NotFound);
    }

    #[test]
    fn navigable_span_covers_the_hovered_token() {
        let sql = "SELECT * FROM dbo.Users";
        let span = navigable_span_at(
            sql,
            mid(sql, "Users"),
            &test_catalog(),
            "dbo",
            Some("appdb"),
        );
        let start = sql.find("Users").unwrap();
        assert_eq!(span, Some((start, start + "Users".len())));
    }

    #[test]
    fn navigable_span_includes_bracket_delimiters() {
        let sql = "SELECT * FROM [dbo].[Users]";
        let span = navigable_span_at(
            sql,
            mid(sql, "[Users]"),
            &test_catalog(),
            "dbo",
            Some("appdb"),
        );
        let start = sql.find("[Users]").unwrap();
        assert_eq!(span, Some((start, start + "[Users]".len())));
    }

    #[test]
    fn navigable_span_none_for_keywords_and_unknowns() {
        let sql = "SELECT * FROM dbo.Users WHERE Nope = 1";
        assert_eq!(
            navigable_span_at(sql, mid(sql, "SELECT"), &test_catalog(), "dbo", None),
            None
        );
        assert_eq!(
            navigable_span_at(sql, mid(sql, "Nope"), &test_catalog(), "dbo", None),
            None
        );
    }

    #[test]
    fn qualified_name_helper() {
        let target = NavigationTarget::Table {
            catalog: None,
            schema: "dbo".into(),
            name: "Users".into(),
            clicked: ClickedRole::Table,
        };
        assert_eq!(target.qualified_name(), "dbo.Users");
    }
}
