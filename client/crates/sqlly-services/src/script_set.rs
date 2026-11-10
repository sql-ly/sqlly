//! Multi-object script assembly: dependency-ordered CREATE scripts and guarded
//! DROP scripts for an explicit list of objects, reusing the dump pipeline's
//! per-object DDL fetches without writing to the filesystem.
//!
//! Bulk explorer operations (multi-select scripting) and the Deploy Cart both
//! build their reviewed scripts here. Ordering is a *stable* dependency sort:
//! tables come first, parents before children by foreign key; views and
//! routines follow, each after the selected views/routines its definition
//! references. Wherever no dependency decides, the caller's order (click
//! order, cart order) is kept. [`render_create_script`] and
//! [`render_drop_script_live`] are the async entry points that fetch live
//! definitions; the ordering and rendering underneath are pure.

use sqlly_core::{ObjectKind, StudioResult};
use sqlly_intel::intellisense::{ClickedRole, NavigationTarget, RoutineKind};
use sqlly_lang::sqlmodel::DatabaseType;

use crate::ddl::{DdlOptions, fetch_ddl_document_with};
use crate::dump_render::{drop_statement, script_definition, table_key, write_separator};
use crate::metadata::ForeignKeyRow;
use crate::services::Services;

/// One object to script, identified by schema + name + engine kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptSetObject {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
}

impl ScriptSetObject {
    pub fn new(schema: impl Into<String>, name: impl Into<String>, kind: ObjectKind) -> Self {
        Self {
            schema: schema.into(),
            name: name.into(),
            kind,
        }
    }

    /// The dump pipeline's qualified label (`schema.name`, or bare `name`).
    pub fn label(&self) -> String {
        table_key(&self.schema, &self.name)
    }

    pub fn is_table(&self) -> bool {
        self.kind == ObjectKind::Table
    }

    fn is_view(&self) -> bool {
        self.kind == ObjectKind::View
    }

    fn is_routine(&self) -> bool {
        matches!(
            self.kind,
            ObjectKind::StoredProcedure
                | ObjectKind::ScalarFunction
                | ObjectKind::TableFunction
                | ObjectKind::AggregateFunction
        )
    }

    fn is_supported(&self) -> bool {
        self.is_table() || self.is_view() || self.is_routine()
    }

    /// Whether a reference `(schema, name)` found in some definition names
    /// this object. Identifier case is ignored (a false match only adds an
    /// ordering constraint); an unqualified reference matches any schema.
    fn is_named_by(&self, schema: Option<&str>, name: &str) -> bool {
        if self.name.to_lowercase() != name.to_lowercase() {
            return false;
        }
        match schema {
            None => true,
            Some(schema) => {
                self.schema.is_empty() || self.schema.to_lowercase() == schema.to_lowercase()
            }
        }
    }
}

/// A generated script plus the non-fatal problems hit while building it
/// (objects whose definition could not be fetched, dependency cycles). The
/// same warnings are repeated as `-- WARNING:` lines in the script header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptSetReport {
    pub sql: String,
    pub warnings: Vec<sqlly_core::DisplayMessage>,
}

/// The DDL-fetch navigation target for an object, mirroring the dump
/// pipeline's mapping (views script like tables; functions/procedures as
/// routines).
fn navigation_target(obj: &ScriptSetObject) -> NavigationTarget {
    match obj.kind {
        ObjectKind::View => NavigationTarget::Table {
            catalog: None,
            schema: obj.schema.clone(),
            name: obj.name.clone(),
            clicked: ClickedRole::View,
        },
        ObjectKind::StoredProcedure => NavigationTarget::Routine {
            catalog: None,
            schema: obj.schema.clone(),
            name: obj.name.clone(),
            kind: RoutineKind::Procedure,
            clicked: ClickedRole::Procedure,
        },
        ObjectKind::ScalarFunction | ObjectKind::TableFunction | ObjectKind::AggregateFunction => {
            NavigationTarget::Routine {
                catalog: None,
                schema: obj.schema.clone(),
                name: obj.name.clone(),
                kind: RoutineKind::Function,
                clicked: ClickedRole::Function,
            }
        }
        _ => NavigationTarget::Table {
            catalog: None,
            schema: obj.schema.clone(),
            name: obj.name.clone(),
            clicked: ClickedRole::Table,
        },
    }
}

/// Text safe to place after `--`: every line break (which would end the
/// comment and turn the rest into live SQL) becomes a space.
pub fn comment_safe(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\r' | '\n' | '\u{2028}' | '\u{2029}' | '\u{85}' => ' ',
            other => other,
        })
        .collect()
}

/// One lexical piece of a definition, for reference scanning.
#[derive(Debug, PartialEq, Eq)]
enum RefToken {
    Ident(String),
    Dot,
    Other,
}

/// Lex `text` into identifiers (bare, `[bracketed]`, `"quoted"`,
/// `` `backticked` `` — unescaped), dots, and everything else. Comments and
/// single-quoted string literals are skipped, so a name mentioned only there
/// never counts as a reference.
fn ref_tokens(text: &str) -> Vec<RefToken> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    let quoted = |close: char, i: &mut usize, tokens: &mut Vec<RefToken>| {
        *i += 1;
        let mut ident = String::new();
        while *i < chars.len() {
            if chars[*i] == close {
                if chars.get(*i + 1) == Some(&close) {
                    ident.push(close);
                    *i += 2;
                    continue;
                }
                *i += 1;
                break;
            }
            ident.push(chars[*i]);
            *i += 1;
        }
        tokens.push(RefToken::Ident(ident));
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                tokens.push(RefToken::Other);
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
                tokens.push(RefToken::Other);
            }
            '\'' => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        if chars.get(i + 1) == Some(&'\'') {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                tokens.push(RefToken::Other);
            }
            '[' => quoted(']', &mut i, &mut tokens),
            '"' => quoted('"', &mut i, &mut tokens),
            '`' => quoted('`', &mut i, &mut tokens),
            '.' => {
                tokens.push(RefToken::Dot);
                i += 1;
            }
            c if c.is_alphabetic() || c == '_' || c == '@' || c == '#' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '$' | '#' | '@'))
                {
                    i += 1;
                }
                tokens.push(RefToken::Ident(chars[start..i].iter().collect()));
            }
            c if c.is_whitespace() => i += 1,
            _ => {
                tokens.push(RefToken::Other);
                i += 1;
            }
        }
    }
    tokens
}

/// Every (optionally schema-qualified) name `text` references: the last two
/// parts of each dotted identifier chain.
fn referenced_names(text: &str) -> Vec<(Option<String>, String)> {
    let tokens = ref_tokens(text);
    let mut refs = Vec::new();
    let mut chain: Vec<String> = Vec::new();
    let mut after_dot = false;
    let flush = |chain: &mut Vec<String>, refs: &mut Vec<(Option<String>, String)>| {
        if let Some(name) = chain.pop() {
            refs.push((chain.pop(), name));
        }
        chain.clear();
    };
    for token in tokens {
        match token {
            RefToken::Ident(ident) => {
                if !after_dot {
                    flush(&mut chain, &mut refs);
                }
                chain.push(ident);
                after_dot = false;
            }
            RefToken::Dot => after_dot = true,
            RefToken::Other => {
                flush(&mut chain, &mut refs);
                after_dot = false;
            }
        }
    }
    flush(&mut chain, &mut refs);
    refs
}

/// The definition without its trailing `-- Privileges` section (PostgreSQL
/// GRANT / REVOKE statements, or the `-- Privileges unavailable` note).
/// The roles named there are not objects, so they must not be scanned for
/// dependencies: a role named like a selected view would otherwise add a
/// bogus ordering edge or a cycle warning.
fn definition_without_privileges(text: &str) -> &str {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']).trim();
        if trimmed == "-- Privileges" || trimmed.starts_with("-- Privileges unavailable") {
            return &text[..offset];
        }
        offset += line.len();
    }
    text
}

/// For each object, the indices of the *other selected views and routines*
/// its definition references (`definitions` is parallel to `objects`; `None`
/// where no definition is known). Tables are never listed — they are always
/// created before any view or routine — and tables' own definitions are not
/// scanned (their order comes from foreign keys). A definition's privileges
/// section names roles, not objects, and is skipped.
pub fn definition_dependencies(
    objects: &[ScriptSetObject],
    definitions: &[Option<&str>],
) -> Vec<Vec<usize>> {
    objects
        .iter()
        .enumerate()
        .map(|(index, obj)| {
            if obj.is_table() || !obj.is_supported() {
                return Vec::new();
            }
            let Some(Some(text)) = definitions.get(index) else {
                return Vec::new();
            };
            let refs = referenced_names(definition_without_privileges(text));
            objects
                .iter()
                .enumerate()
                .filter(|(other, candidate)| {
                    *other != index
                        && !candidate.is_table()
                        && candidate.is_supported()
                        && refs
                            .iter()
                            .any(|(schema, name)| candidate.is_named_by(schema.as_deref(), name))
                })
                .map(|(other, _)| other)
                .collect()
        })
        .collect()
}

/// Stable topological order over `members` (indices into the caller's list,
/// in the caller's order): repeatedly place the earliest member whose
/// dependencies are all placed. `deps(i)` lists what `i` must follow;
/// dependencies outside `members` are ignored. Returns the order and whether
/// a cycle forced a member out early.
fn stable_topo(members: &[usize], deps: impl Fn(usize) -> Vec<usize>) -> (Vec<usize>, bool) {
    let mut placed: Vec<usize> = Vec::with_capacity(members.len());
    let mut remaining: Vec<usize> = members.to_vec();
    let mut cycle = false;
    while !remaining.is_empty() {
        let ready = remaining.iter().position(|&m| {
            deps(m)
                .iter()
                .all(|d| *d == m || !members.contains(d) || placed.contains(d))
        });
        let pick = match ready {
            Some(position) => position,
            None => {
                // Cycle: keep going in caller order rather than stall.
                cycle = true;
                0
            }
        };
        placed.push(remaining.remove(pick));
    }
    (placed, cycle)
}

/// Order objects for a CREATE script with no definition-level dependency
/// information: see [`order_for_create_with_deps`].
pub fn order_for_create(
    objects: &[ScriptSetObject],
    fks: &[ForeignKeyRow],
) -> (Vec<ScriptSetObject>, Vec<sqlly_core::DisplayMessage>) {
    order_for_create_with_deps(objects, fks, &[])
}

/// Order objects for a CREATE script: tables first, each after the selected
/// tables it references by foreign key; then views and routines, each after
/// the selected views/routines listed for it in `deps` (parallel to
/// `objects`, from [`definition_dependencies`]). Ties keep the caller's
/// order. Objects of any other kind trail the list and earn a warning.
pub fn order_for_create_with_deps(
    objects: &[ScriptSetObject],
    fks: &[ForeignKeyRow],
    deps: &[Vec<usize>],
) -> (Vec<ScriptSetObject>, Vec<sqlly_core::DisplayMessage>) {
    let mut warnings = Vec::new();

    let tables: Vec<usize> = (0..objects.len())
        .filter(|&i| objects[i].is_table())
        .collect();
    let (table_order, table_cycle) = stable_topo(&tables, |i| {
        let child = &objects[i];
        fks.iter()
            .filter(|fk| fk.from_schema == child.schema && fk.from_table == child.name)
            .filter_map(|fk| {
                tables
                    .iter()
                    .copied()
                    .find(|&t| objects[t].schema == fk.to_schema && objects[t].name == fk.to_table)
            })
            .collect()
    });
    if table_cycle {
        warnings.push(sqlly_core::DisplayMessage::new(
            "services.script_set.warnings.foreign_key_cycle",
            "foreign-key cycle among the selected tables; they keep your order where the cycle leaves no choice",
        ));
    }

    let programmable: Vec<usize> = (0..objects.len())
        .filter(|&i| !objects[i].is_table() && objects[i].is_supported())
        .collect();
    let (program_order, program_cycle) =
        stable_topo(&programmable, |i| deps.get(i).cloned().unwrap_or_default());
    if program_cycle {
        warnings.push(sqlly_core::DisplayMessage::new(
            "services.script_set.warnings.dependency_cycle",
            "dependency cycle among the selected views and routines; they keep your order where the cycle leaves no choice",
        ));
    }

    let mut ordered: Vec<ScriptSetObject> = Vec::with_capacity(objects.len());
    ordered.extend(table_order.iter().map(|&i| objects[i].clone()));
    ordered.extend(program_order.iter().map(|&i| objects[i].clone()));
    for obj in objects.iter().filter(|o| !o.is_supported()) {
        warnings.push(
            sqlly_core::DisplayMessage::new(
                "services.script_set.warnings.unsupported_object",
                format!(
                    "{}: scripting {:?} objects is not supported",
                    obj.label(),
                    obj.kind
                ),
            )
            .with_arg("object_name", obj.label())
            .with_arg("object_type", format!("{:?}", obj.kind)),
        );
        ordered.push(obj.clone());
    }
    (ordered, warnings)
}

/// Order objects for a DROP script with no definition-level dependency
/// information: see [`order_for_drop_with_deps`].
pub fn order_for_drop(
    objects: &[ScriptSetObject],
    fks: &[ForeignKeyRow],
) -> (Vec<ScriptSetObject>, Vec<sqlly_core::DisplayMessage>) {
    order_for_drop_with_deps(objects, fks, &[])
}

/// Order objects for a DROP script: the exact reverse of the CREATE order,
/// so views and routines drop before tables, anything that depends on
/// another selected object drops before it, and child tables drop before
/// their foreign-key parents. Unsupported kinds (skipped when rendering)
/// trail the list.
pub fn order_for_drop_with_deps(
    objects: &[ScriptSetObject],
    fks: &[ForeignKeyRow],
    deps: &[Vec<usize>],
) -> (Vec<ScriptSetObject>, Vec<sqlly_core::DisplayMessage>) {
    let (created, warnings) = order_for_create_with_deps(objects, fks, deps);
    let mut ordered: Vec<ScriptSetObject> = created
        .iter()
        .filter(|o| o.is_supported())
        .rev()
        .cloned()
        .collect();
    ordered.extend(created.iter().filter(|o| !o.is_supported()).cloned());
    (ordered, warnings)
}

fn script_header(
    action: &str,
    database: &str,
    count: usize,
    warnings: &[sqlly_core::DisplayMessage],
) -> String {
    let mut header = format!(
        "-- SQLLY: {action} for {count} object(s) in [{}]\n-- Review carefully before running.\n",
        comment_safe(database)
    );
    for warning in warnings {
        header.push_str(&format!("-- WARNING: {}\n", comment_safe(warning)));
    }
    header.push('\n');
    header
}

/// A guarded `DROP … IF EXISTS` script for every object, in dependency-safe
/// order. Pure rendering — no server I/O — so the caller supplies the FK rows
/// used to order the table drops and (optionally) the definition-level
/// dependencies from [`definition_dependencies`].
pub fn render_drop_script(
    db: DatabaseType,
    database: &str,
    objects: &[ScriptSetObject],
    fks: &[ForeignKeyRow],
) -> ScriptSetReport {
    render_drop_script_with_deps(db, database, objects, fks, &[], Vec::new())
}

fn render_drop_script_with_deps(
    db: DatabaseType,
    database: &str,
    objects: &[ScriptSetObject],
    fks: &[ForeignKeyRow],
    deps: &[Vec<usize>],
    mut warnings: Vec<sqlly_core::DisplayMessage>,
) -> ScriptSetReport {
    let (ordered, order_warnings) = order_for_drop_with_deps(objects, fks, deps);
    warnings.extend(order_warnings);
    let mut body = String::new();
    for obj in ordered.iter().filter(|o| o.is_supported()) {
        body.push_str(&drop_statement(
            db,
            obj.kind.clone(),
            Some(&obj.schema),
            &obj.name,
        ));
        body.push('\n');
        write_separator(&mut body, db);
    }
    let mut sql = script_header("DROP script", database, objects.len(), &warnings);
    sql.push_str(&body);
    ScriptSetReport { sql, warnings }
}

/// The source database's foreign keys, or none plus a warning when the
/// engine cannot list them.
async fn foreign_keys_or_warn(
    services: &Services,
    profile: &sqlly_core::ConnectionProfile,
    database: &str,
    warnings: &mut Vec<sqlly_core::DisplayMessage>,
) -> StudioResult<Vec<ForeignKeyRow>> {
    match services.metadata.list_foreign_keys(profile, database).await {
        Ok(fks) => Ok(fks),
        Err(sqlly_core::StudioError::NotImplemented(_)) => {
            warnings.push(sqlly_core::DisplayMessage::new(
                "services.script_set.warnings.foreign_keys_unavailable",
                "foreign keys are unavailable on this engine, so table order follows your selection order",
            ));
            Ok(Vec::new())
        }
        Err(err) => Err(err),
    }
}

/// A fetched definition: its body, or why it could not be read.
type Definition = Result<String, String>;

/// Fetch the live definition of every supported object (`None` for
/// unsupported kinds and, when `tables` is false, for tables). A failure —
/// transport or "no definition" — is recorded per object, never aborting.
async fn fetch_definitions(
    services: &Services,
    profile: &sqlly_core::ConnectionProfile,
    database: &str,
    objects: &[ScriptSetObject],
    tables: bool,
) -> Vec<Option<Definition>> {
    fetch_definitions_with(
        services,
        profile,
        database,
        objects,
        tables,
        DdlOptions::default(),
    )
    .await
}

async fn fetch_definitions_with(
    services: &Services,
    profile: &sqlly_core::ConnectionProfile,
    database: &str,
    objects: &[ScriptSetObject],
    tables: bool,
    options: DdlOptions,
) -> Vec<Option<Definition>> {
    let mut out = Vec::with_capacity(objects.len());
    for obj in objects {
        if !obj.is_supported() || (obj.is_table() && !tables) {
            out.push(None);
            continue;
        }
        let fetched = fetch_ddl_document_with(
            services.queries.as_ref(),
            profile,
            database,
            &navigation_target(obj),
            options,
        )
        .await;
        out.push(Some(match fetched {
            Err(err) => Err(err.to_string()),
            Ok(document) => match (document.error.clone(), document.primary_source()) {
                (Some(err), _) => Err(err),
                (None, Some(source)) => Ok(source.body.clone()),
                (None, None) => Err("no definition available".to_string()),
            },
        }));
    }
    out
}

fn dependencies_from(
    objects: &[ScriptSetObject],
    fetched: &[Option<Definition>],
) -> Vec<Vec<usize>> {
    let bodies: Vec<Option<&str>> = fetched
        .iter()
        .map(|d| d.as_ref().and_then(|d| d.as_ref().ok()).map(String::as_str))
        .collect();
    definition_dependencies(objects, &bodies)
}

/// A guarded DROP script ordered with live dependency information: the
/// source's foreign keys order the tables, and the selected views' and
/// routines' definitions (fetched read-only) order those. Objects whose
/// definition cannot be read are still dropped; a header warning says their
/// position may not respect what depends on them.
pub async fn render_drop_script_live(
    services: &Services,
    profile: &sqlly_core::ConnectionProfile,
    database: &str,
    objects: &[ScriptSetObject],
) -> StudioResult<ScriptSetReport> {
    let mut warnings = Vec::new();
    let fks = foreign_keys_or_warn(services, profile, database, &mut warnings).await?;
    let fetched = fetch_definitions(services, profile, database, objects, false).await;
    for (obj, definition) in objects.iter().zip(&fetched) {
        if let Some(Err(err)) = definition {
            warnings.push(
                sqlly_core::DisplayMessage::new(
                    "services.script_set.warnings.drop_definition_unavailable",
                    format!("{}: definition unavailable ({err}), so its drop position may not respect objects that depend on it", obj.label()),
                )
                .with_arg("object_name", obj.label())
                .with_arg("detail", err.clone()),
            );
        }
    }
    let deps = dependencies_from(objects, &fetched);
    Ok(render_drop_script_with_deps(
        profile.database_type,
        database,
        objects,
        &fks,
        &deps,
        warnings,
    ))
}

/// The CREATE order [`render_create_script`] would use, without rendering:
/// tables by the source's foreign keys, views and routines by their live
/// definitions' references to each other, ties in the caller's order. Used to
/// number a Deploy Cart's per-object files in the same order as its single
/// script. Only view and routine definitions are fetched (tables order by
/// foreign key alone); one that can't be read is reported as a warning and
/// keeps the caller's position.
pub async fn order_for_create_live(
    services: &Services,
    profile: &sqlly_core::ConnectionProfile,
    database: &str,
    objects: &[ScriptSetObject],
) -> StudioResult<(Vec<ScriptSetObject>, Vec<sqlly_core::DisplayMessage>)> {
    let mut warnings = Vec::new();
    let fks = foreign_keys_or_warn(services, profile, database, &mut warnings).await?;
    let fetched = fetch_definitions(services, profile, database, objects, false).await;
    for (obj, definition) in objects.iter().zip(&fetched) {
        if let Some(Err(err)) = definition {
            warnings.push(
                sqlly_core::DisplayMessage::new(
                    "services.script_set.warnings.file_definition_unavailable",
                    format!("{}: definition unavailable ({err}), so its file position may not respect objects it depends on", obj.label()),
                )
                .with_arg("object_name", obj.label())
                .with_arg("detail", err.clone()),
            );
        }
    }
    let deps = dependencies_from(objects, &fetched);
    let (ordered, order_warnings) = order_for_create_with_deps(objects, &fks, &deps);
    warnings.extend(order_warnings);
    Ok((ordered, warnings))
}

/// Fetch the live CREATE definition of every object and concatenate one
/// dependency-ordered script. Objects whose definition fetch fails (including
/// transport errors) are written as `-- SKIPPED` comments and reported as
/// warnings, never aborting the run.
pub async fn render_create_script(
    services: &Services,
    profile: &sqlly_core::ConnectionProfile,
    database: &str,
    objects: &[ScriptSetObject],
    drop_if_exists: bool,
) -> StudioResult<ScriptSetReport> {
    render_create_script_with(
        services,
        profile,
        database,
        objects,
        drop_if_exists,
        DdlOptions::default(),
    )
    .await
}

/// [`render_create_script`] with [`DdlOptions`] applied to every object's
/// definition (e.g. PostgreSQL privileges after each CREATE).
pub async fn render_create_script_with(
    services: &Services,
    profile: &sqlly_core::ConnectionProfile,
    database: &str,
    objects: &[ScriptSetObject],
    drop_if_exists: bool,
    options: DdlOptions,
) -> StudioResult<ScriptSetReport> {
    let db = profile.database_type;
    let mut warnings = Vec::new();
    let fks = foreign_keys_or_warn(services, profile, database, &mut warnings).await?;
    let fetched = fetch_definitions_with(services, profile, database, objects, true, options).await;
    let deps = dependencies_from(objects, &fetched);
    let (ordered, order_warnings) = order_for_create_with_deps(objects, &fks, &deps);
    warnings.extend(order_warnings);

    let mut body = String::new();
    for obj in ordered.iter().filter(|o| o.is_supported()) {
        let index = objects
            .iter()
            .position(|o| o == obj)
            .expect("ordered objects come from the input");
        if drop_if_exists {
            body.push_str(&drop_statement(
                db,
                obj.kind.clone(),
                Some(&obj.schema),
                &obj.name,
            ));
            body.push('\n');
            write_separator(&mut body, db);
        }
        match &fetched[index] {
            Some(Ok(source)) => {
                let text = script_definition(db, &obj.kind, source);
                body.push_str(&text);
                if !text.ends_with('\n') {
                    body.push('\n');
                }
            }
            Some(Err(err)) => {
                warnings.push(
                    sqlly_core::DisplayMessage::new(
                        "services.script_set.warnings.definition_unavailable",
                        format!("{}: {err}", obj.label()),
                    )
                    .with_arg("object_name", obj.label())
                    .with_arg("detail", err.clone()),
                );
                body.push_str(&format!(
                    "-- SKIPPED {}: {}\n",
                    comment_safe(&obj.label()),
                    comment_safe(err)
                ));
            }
            None => {}
        }
        write_separator(&mut body, db);
    }
    let mut sql = script_header("CREATE script", database, objects.len(), &warnings);
    sql.push_str(&body);
    Ok(ScriptSetReport { sql, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(schema: &str, name: &str, kind: ObjectKind) -> ScriptSetObject {
        ScriptSetObject::new(schema, name, kind)
    }

    fn table(schema: &str, name: &str) -> ScriptSetObject {
        obj(schema, name, ObjectKind::Table)
    }

    fn fk(from: (&str, &str), to: (&str, &str)) -> ForeignKeyRow {
        ForeignKeyRow {
            constraint_name: format!("FK_{}_{}", from.1, to.1),
            from_schema: from.0.to_string(),
            from_table: from.1.to_string(),
            from_column: "id".to_string(),
            to_schema: to.0.to_string(),
            to_table: to.1.to_string(),
            to_column: "id".to_string(),
        }
    }

    fn labels(objects: &[ScriptSetObject]) -> Vec<String> {
        objects.iter().map(|o| o.label()).collect()
    }

    #[test]
    fn create_order_puts_fk_parents_first_then_programmables_in_caller_order() {
        let objects = vec![
            obj("dbo", "p_rebuild", ObjectKind::StoredProcedure),
            obj("dbo", "v_orders", ObjectKind::View),
            table("dbo", "orders"),
            table("dbo", "customers"),
        ];
        let fks = vec![fk(("dbo", "orders"), ("dbo", "customers"))];
        let (ordered, warnings) = order_for_create(&objects, &fks);
        assert!(warnings.is_empty());
        assert_eq!(
            labels(&ordered),
            vec![
                "dbo.customers",
                "dbo.orders",
                "dbo.p_rebuild",
                "dbo.v_orders"
            ]
        );
    }

    #[test]
    fn create_order_keeps_caller_order_where_no_dependency_decides() {
        // Previously alphabetical: the cart's reorder arrows had no effect.
        let objects = vec![
            table("dbo", "zebra"),
            table("dbo", "apple"),
            obj("dbo", "v_z", ObjectKind::View),
            obj("dbo", "v_a", ObjectKind::View),
        ];
        let (ordered, _) = order_for_create(&objects, &[]);
        assert_eq!(
            labels(&ordered),
            vec!["dbo.zebra", "dbo.apple", "dbo.v_z", "dbo.v_a"]
        );
    }

    #[test]
    fn create_order_tolerates_cycles_and_unknown_kinds() {
        let objects = vec![
            table("dbo", "a"),
            table("dbo", "b"),
            obj("dbo", "ix_a", ObjectKind::Index),
        ];
        let fks = vec![
            fk(("dbo", "a"), ("dbo", "b")),
            fk(("dbo", "b"), ("dbo", "a")),
        ];
        let (ordered, warnings) = order_for_create(&objects, &fks);
        assert_eq!(ordered.len(), 3);
        assert!(warnings.iter().any(|w| w.contains("cycle")));
        assert!(warnings.iter().any(|w| w.contains("dbo.ix_a")));
        // The unsupported kind trails the tables.
        assert_eq!(ordered.last().unwrap().label(), "dbo.ix_a");
    }

    #[test]
    fn view_over_view_and_view_calling_function_create_after_their_dependencies() {
        let objects = vec![
            obj("dbo", "v_top", ObjectKind::View),
            obj("dbo", "v_base", ObjectKind::View),
            obj("dbo", "fn_tax", ObjectKind::ScalarFunction),
            table("dbo", "orders"),
        ];
        let definitions = vec![
            Some("CREATE VIEW dbo.v_top AS SELECT * FROM [dbo].[v_base] WHERE x > 1"),
            Some("CREATE VIEW v_base AS SELECT id, dbo.fn_tax(total) AS t FROM dbo.orders"),
            Some("CREATE FUNCTION dbo.fn_tax(@x money) RETURNS money AS BEGIN RETURN @x END"),
            None,
        ];
        let deps = definition_dependencies(&objects, &definitions);
        assert_eq!(deps[0], vec![1], "v_top reads v_base");
        assert_eq!(
            deps[1],
            vec![2],
            "v_base calls fn_tax (tables never listed)"
        );
        assert!(deps[2].is_empty());
        let (ordered, warnings) = order_for_create_with_deps(&objects, &[], &deps);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            labels(&ordered),
            vec!["dbo.orders", "dbo.fn_tax", "dbo.v_base", "dbo.v_top"]
        );
        let (dropped, _) = order_for_drop_with_deps(&objects, &[], &deps);
        assert_eq!(
            labels(&dropped),
            vec!["dbo.v_top", "dbo.v_base", "dbo.fn_tax", "dbo.orders"]
        );
    }

    #[test]
    fn references_in_comments_strings_and_other_schemas_do_not_count() {
        let objects = vec![
            obj("sales", "v_report", ObjectKind::View),
            obj("sales", "v_base", ObjectKind::View),
        ];
        let definitions = vec![
            Some(
                "CREATE VIEW sales.v_report AS\n-- v_base is not used\n/* v_base */ SELECT 'v_base' AS s FROM hr.v_base",
            ),
            Some("CREATE VIEW sales.v_base AS SELECT 1 AS x"),
        ];
        let deps = definition_dependencies(&objects, &definitions);
        assert!(deps[0].is_empty(), "{:?}", deps[0]);
        // Quoted, differently-cased, schema-qualified references do count.
        let definitions = vec![Some("SELECT * FROM \"SALES\".\"V_BASE\""), None];
        assert_eq!(definition_dependencies(&objects, &definitions)[0], vec![1]);
    }

    #[test]
    fn roles_in_the_privileges_section_are_not_dependencies() {
        // A role named like a selected view, granted on another view: the
        // GRANT must not count as a reference (it would make a cycle).
        let objects = vec![
            obj("app", "reporting", ObjectKind::View),
            obj("app", "v_sales", ObjectKind::View),
        ];
        let definitions = vec![
            Some(
                "CREATE VIEW app.reporting AS SELECT 1 AS x;\n\n-- Privileges\nGRANT SELECT ON TABLE app.reporting TO v_sales;",
            ),
            Some(
                "CREATE VIEW app.v_sales AS SELECT * FROM app.reporting;\n\n-- Privileges\nREVOKE SELECT ON TABLE app.v_sales FROM reporting;\nGRANT SELECT ON TABLE app.v_sales TO \"reporting\";",
            ),
        ];
        let deps = definition_dependencies(&objects, &definitions);
        assert!(deps[0].is_empty(), "{:?}", deps[0]);
        assert_eq!(deps[1], vec![0], "the body's reference still counts");
        let (ordered, warnings) = order_for_create_with_deps(&objects, &[], &deps);
        assert_eq!(labels(&ordered), vec!["app.reporting", "app.v_sales"]);
        assert!(warnings.is_empty(), "{warnings:?}");

        // The unavailable note, CRLF endings, and a marker that is not on a
        // line of its own.
        assert_eq!(
            definition_without_privileges("SELECT 1;\r\n\r\n-- Privileges unavailable: denied\r\n"),
            "SELECT 1;\r\n\r\n"
        );
        assert_eq!(
            definition_without_privileges("SELECT 1; -- Privileges are fine\nSELECT 2"),
            "SELECT 1; -- Privileges are fine\nSELECT 2"
        );
        assert_eq!(definition_without_privileges("-- Privileges\nGRANT"), "");
    }

    #[test]
    fn view_cycles_fall_back_to_caller_order_with_a_warning() {
        let objects = vec![
            obj("dbo", "a", ObjectKind::View),
            obj("dbo", "b", ObjectKind::View),
        ];
        let definitions = vec![Some("SELECT * FROM b"), Some("SELECT * FROM a")];
        let deps = definition_dependencies(&objects, &definitions);
        let (ordered, warnings) = order_for_create_with_deps(&objects, &[], &deps);
        assert_eq!(labels(&ordered), vec!["dbo.a", "dbo.b"]);
        assert!(warnings.iter().any(|w| w.contains("cycle")));
    }

    #[test]
    fn drop_order_is_views_and_routines_first_then_tables_children_first() {
        let objects = vec![
            table("dbo", "orders"),
            table("dbo", "customers"),
            obj("dbo", "v_orders", ObjectKind::View),
        ];
        let fks = vec![fk(("dbo", "orders"), ("dbo", "customers"))];
        let (ordered, _) = order_for_drop(&objects, &fks);
        assert_eq!(
            labels(&ordered),
            vec!["dbo.v_orders", "dbo.orders", "dbo.customers"]
        );
    }

    #[test]
    fn comment_text_cannot_break_out_of_a_line_comment() {
        assert_eq!(
            comment_safe("a\nDROP TABLE t;\r\n--"),
            "a DROP TABLE t;  --"
        );
        let report = render_drop_script(
            DatabaseType::SqlServer,
            "shop\nDROP DATABASE shop",
            &[table("dbo", "t")],
            &[],
        );
        assert!(
            report
                .sql
                .lines()
                .all(|line| !line.starts_with("DROP DATABASE")),
            "{}",
            report.sql
        );
    }

    #[test]
    fn drop_script_renders_guarded_statements_in_order() {
        let objects = vec![
            table("dbo", "orders"),
            table("dbo", "customers"),
            obj("dbo", "v_orders", ObjectKind::View),
        ];
        let fks = vec![fk(("dbo", "orders"), ("dbo", "customers"))];
        let report = render_drop_script(DatabaseType::SqlServer, "shop", &objects, &fks);
        assert!(report.warnings.is_empty());
        let view_pos = report
            .sql
            .find("DROP VIEW IF EXISTS [dbo].[v_orders];")
            .unwrap();
        let orders_pos = report
            .sql
            .find("DROP TABLE IF EXISTS [dbo].[orders];")
            .unwrap();
        let customers_pos = report
            .sql
            .find("DROP TABLE IF EXISTS [dbo].[customers];")
            .unwrap();
        assert!(view_pos < orders_pos && orders_pos < customers_pos);
        assert!(report.sql.contains("Review carefully before running"));
    }

    #[test]
    fn drop_script_skips_unsupported_kinds_with_a_warning() {
        let objects = vec![table("dbo", "t"), obj("dbo", "ix_t", ObjectKind::Index)];
        let report = render_drop_script(DatabaseType::Postgres, "db", &objects, &[]);
        assert!(report.sql.contains("DROP TABLE IF EXISTS \"dbo\".\"t\";"));
        assert!(!report.sql.contains("ix_t\";"));
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("dbo.ix_t"));
    }

    /// First call (the foreign-key listing) succeeds empty; every later call
    /// (the definition fetches) fails at the transport level.
    struct FlakyQueries {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::query::QueryExecutor for FlakyQueries {
        async fn execute(
            &self,
            _profile: &sqlly_core::ConnectionProfile,
            _sql: &str,
            _row_buffer: usize,
        ) -> StudioResult<crate::query::QueryHandle> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                return Err(sqlly_core::StudioError::Connection(
                    "connection reset\nDROP TABLE t".into(),
                ));
            }
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            tx.send(crate::query::QueryEvent::Completed { elapsed_ms: 1 })
                .await
                .ok();
            Ok(crate::query::QueryHandle {
                task_id: "t".into(),
                events: rx,
            })
        }

        async fn cancel(&self, _task_id: &str) -> StudioResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn transport_errors_skip_the_object_instead_of_aborting_the_script() {
        let queries: std::sync::Arc<dyn crate::query::QueryExecutor> =
            std::sync::Arc::new(FlakyQueries {
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
        let mut services = Services::new();
        services.metadata = std::sync::Arc::new(crate::metadata::DefaultMetadataService::new(
            queries.clone(),
        ));
        services.queries = queries;
        let profile = sqlly_core::ConnectionProfile::sql_login(
            "t",
            "localhost",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        let objects = vec![table("dbo", "a"), obj("dbo", "v", ObjectKind::View)];
        let report = render_create_script(&services, &profile, "shop", &objects, false)
            .await
            .expect("a failed definition fetch must not abort the script");
        assert!(report.sql.contains("-- SKIPPED dbo.a:"), "{}", report.sql);
        assert!(report.sql.contains("-- SKIPPED dbo.v:"), "{}", report.sql);
        assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
        // The error text stays inside its comment.
        assert!(
            report
                .sql
                .lines()
                .all(|line| !line.trim_start().starts_with("DROP TABLE")),
            "{}",
            report.sql
        );
    }

    /// Answers each query with the rows of the first route whose needle it
    /// contains; anything else completes empty.
    struct RoutedQueries {
        routes: Vec<(&'static str, Vec<Vec<sqlly_core::CellValue>>)>,
    }

    #[async_trait::async_trait]
    impl crate::query::QueryExecutor for RoutedQueries {
        async fn execute(
            &self,
            _profile: &sqlly_core::ConnectionProfile,
            sql: &str,
            _row_buffer: usize,
        ) -> StudioResult<crate::query::QueryHandle> {
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            if let Some((_, rows)) = self.routes.iter().find(|(needle, _)| sql.contains(needle)) {
                tx.send(crate::query::QueryEvent::Rows {
                    index: 0,
                    rows: rows.clone(),
                })
                .await
                .ok();
            }
            tx.send(crate::query::QueryEvent::Completed { elapsed_ms: 1 })
                .await
                .ok();
            Ok(crate::query::QueryHandle {
                task_id: "t".into(),
                events: rx,
            })
        }

        async fn cancel(&self, _task_id: &str) -> StudioResult<()> {
            Ok(())
        }
    }

    fn services_with(queries: RoutedQueries) -> Services {
        let queries: std::sync::Arc<dyn crate::query::QueryExecutor> = std::sync::Arc::new(queries);
        let mut services = Services::new();
        services.metadata = std::sync::Arc::new(crate::metadata::DefaultMetadataService::new(
            queries.clone(),
        ));
        services.queries = queries;
        services
    }

    /// A MySQL routine body holds `;`-terminated statements, so the script
    /// must switch the client delimiter around it or a restore stops at the
    /// first inner `;` with ERROR 1064.
    #[tokio::test]
    async fn mysql_routine_with_inner_semicolons_is_wrapped_in_delimiter() {
        let services = services_with(RoutedQueries {
            routes: vec![(
                "information_schema.routines",
                vec![vec![sqlly_core::CellValue::String(
                    "CREATE PROCEDURE `shop`.`touch`()\nBEGIN\n  UPDATE t SET x = 1;\n  SELECT 2;\nEND"
                        .into(),
                )]],
            )],
        });
        let mut profile = sqlly_core::ConnectionProfile::sql_login(
            "t",
            "localhost",
            "root",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        profile.database_type = DatabaseType::MySql;
        let objects = vec![obj("shop", "touch", ObjectKind::StoredProcedure)];
        let report = render_create_script(&services, &profile, "shop", &objects, true)
            .await
            .unwrap();
        let sql = &report.sql;
        assert!(sql.contains("DELIMITER $$\nCREATE PROCEDURE"), "{sql}");
        assert!(sql.contains("END$$\nDELIMITER ;\n"), "{sql}");
        let statements =
            sqlly_lang::sqllex::split_statements(sql, sqlly_lang::sqlmodel::Dialect::MySql);
        let texts: Vec<&str> = statements.iter().map(|s| s.text.trim()).collect();
        assert!(
            texts.iter().any(|t| t.contains("CREATE PROCEDURE")
                && t.contains("SELECT 2")
                && t.ends_with("END")),
            "{texts:#?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("DROP PROCEDURE IF EXISTS")),
            "{texts:#?}"
        );
    }

    /// The file-export order: tables lead, an unreadable view definition is a
    /// warning (never an abort) and keeps its cart position, and the caller's
    /// order breaks ties exactly as in the single script.
    #[tokio::test]
    async fn live_create_order_survives_unreadable_definitions() {
        let queries: std::sync::Arc<dyn crate::query::QueryExecutor> =
            std::sync::Arc::new(FlakyQueries {
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
        let mut services = Services::new();
        services.metadata = std::sync::Arc::new(crate::metadata::DefaultMetadataService::new(
            queries.clone(),
        ));
        services.queries = queries;
        let profile = sqlly_core::ConnectionProfile::sql_login(
            "t",
            "localhost",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        let objects = vec![
            obj("dbo", "v_z", ObjectKind::View),
            table("dbo", "zebra"),
            obj("dbo", "v_a", ObjectKind::View),
            table("dbo", "apple"),
        ];
        let (ordered, warnings) = order_for_create_live(&services, &profile, "shop", &objects)
            .await
            .expect("an unreadable definition must not abort the ordering");
        assert_eq!(
            labels(&ordered),
            vec!["dbo.zebra", "dbo.apple", "dbo.v_z", "dbo.v_a"]
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .all(|w| w.contains("definition unavailable"))
        );
    }
}
