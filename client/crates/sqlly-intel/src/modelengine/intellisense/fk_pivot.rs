//! Foreign-key value pivot (§8.3): when a value slot's column is a foreign
//! key, completion offers the referenced table's (key, display) pairs, so the
//! user searches by a readable name and the id is inserted. Pure snapshot
//! reasoning with no I/O, so it is shared by the native model engine's value
//! mode and the editor's value lookup (which also runs in the browser build).

use sqlly_lang::sqlmodel::{
    CatalogName, Dialect, ModelSnapshot, NamespaceName, ObjectKind, ObjectRef, SqlName, TableBody,
    TypeClass,
};

/// Where a foreign-key id column's values come from: the referenced table,
/// its key column, and the human-readable column shown in place of the id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkValuePivot {
    /// The referenced table's canonical reference (schema-qualified when the
    /// model knows the schema), not the name as written in the FK.
    pub target: ObjectRef,
    /// The FK target as written in the constraint, for notes and details.
    pub target_as_written: String,
    pub key_column: String,
    pub display_column: String,
    /// Type class of the key column, for rendering the inserted literal.
    pub key_type: Option<TypeClass>,
}

/// Resolve the pivot for `member` of `table`, or `Ok(None)` when the column is
/// not a foreign key (or its target is not in the model). An explicit
/// `display_override` that names no column on the target is an error.
pub fn resolve_fk_pivot(
    snapshot: &ModelSnapshot,
    table: &TableBody,
    member: &SqlName,
    display_override: Option<&str>,
    dialect: Dialect,
    default_catalog: Option<&CatalogName>,
    default_namespace: Option<&NamespaceName>,
) -> Result<Option<FkValuePivot>, String> {
    let Some(fk) = table.foreign_key_for_column(member) else {
        return Ok(None);
    };
    let Some(target_entry) = snapshot.resolve_kind(
        &fk.target,
        ObjectKind::Table,
        default_catalog,
        default_namespace,
    ) else {
        return Ok(None);
    };
    let Some(target_table) = target_entry.table() else {
        return Ok(None);
    };
    // The referenced column paired with `member` in a composite key, else
    // the first referenced column, else the target's primary key.
    let paired = fk
        .columns
        .iter()
        .position(|c| c.matches(member))
        .and_then(|i| fk.target_columns.get(i).cloned());
    let key_col = paired
        .or_else(|| fk.target_columns.first().cloned())
        .or_else(|| {
            target_table
                .primary_key
                .as_ref()
                .and_then(|pk| pk.columns.first().cloned())
        })
        .ok_or("foreign key target has no key column")?;
    let display = resolve_display_column(
        target_table,
        target_entry.reference.name.spelling(),
        &key_col,
        display_override,
        dialect,
    )?;
    let key_type = target_table
        .column(&key_col)
        .and_then(|c| c.ty.as_ref())
        .map(|t| t.class);
    Ok(Some(FkValuePivot {
        target: target_entry.reference.clone(),
        target_as_written: fk.target.display(),
        key_column: key_col.spelling().to_string(),
        display_column: display,
        key_type,
    }))
}

/// The pivot for a value slot named by plain text (the editor's value lookup:
/// optional schema, table, column), or `None` when the column is not a
/// foreign key the model can follow.
pub fn fk_value_pivot_for(
    snapshot: &ModelSnapshot,
    schema: Option<&str>,
    table: &str,
    column: &str,
    dialect: Dialect,
) -> Option<FkValuePivot> {
    let reference = ObjectRef::new(
        None,
        schema
            .filter(|s| !s.is_empty())
            .map(|s| SqlName::new(s, dialect, false)),
        SqlName::new(table, dialect, false),
    );
    let entry = snapshot.resolve_kind(&reference, ObjectKind::Table, None, None)?;
    let body = entry.table()?;
    let member = body
        .columns
        .iter()
        .find(|c| c.name.spelling().eq_ignore_ascii_case(column))
        .map(|c| c.name.clone())?;
    resolve_fk_pivot(snapshot, body, &member, None, dialect, None, None)
        .ok()
        .flatten()
}

/// The declared type of `column` on `table` (schema-qualified when known),
/// or `None` when the model has no such table/column or no type for it.
pub fn column_type_for(
    snapshot: &ModelSnapshot,
    schema: Option<&str>,
    table: &str,
    column: &str,
    dialect: Dialect,
) -> Option<sqlly_lang::sqlmodel::TypeRef> {
    let reference = ObjectRef::new(
        None,
        schema
            .filter(|s| !s.is_empty())
            .map(|s| SqlName::new(s, dialect, false)),
        SqlName::new(table, dialect, false),
    );
    let entry = snapshot.resolve_kind(&reference, ObjectKind::Table, None, None)?;
    entry
        .table()?
        .columns
        .iter()
        .find(|c| c.name.spelling().eq_ignore_ascii_case(column))?
        .ty
        .clone()
}

/// The text a pivoted value inserts: the key literal followed by the display
/// value as a block comment, so the statement stays readable
/// (`CategoryID = 1 /* Beverages */`).
pub fn fk_insert_text(key_literal: &str, display: &str) -> String {
    format!("{key_literal} /* {} */", sanitize_comment(display))
}

/// Neutralize block-comment delimiters in a display value so it cannot
/// prematurely terminate (or nest inside) the `/* … */` it is wrapped in.
pub(crate) fn sanitize_comment(display: &str) -> String {
    display.replace("*/", "* /").replace("/*", "/ *")
}

/// §8.3 display-column resolution, first match wins:
/// 1. the explicit `requested` column;
/// 2. a column called `name`;
/// 3. the table's own name column: `<Table>Name` / `<Table>_Name`, also for
///    the singular of a plural table (`Categories` → `CategoryName`);
/// 4. the first non-key character column whose name ends in `name`
///    (`CompanyName`, `LastName`);
/// 5. the conventional `label`, `display`, `description`, `code`;
/// 6. the first non-key character column;
/// 7. the key itself.
///
/// Names compare case-insensitively on their spelling, so a quoted mixed-case
/// column (`"CategoryName"` on PostgreSQL) still matches.
pub fn resolve_display_column(
    table: &TableBody,
    table_name: &str,
    key_col: &SqlName,
    requested: Option<&str>,
    dialect: Dialect,
) -> Result<String, String> {
    if let Some(name) = requested {
        let n = SqlName::new(name, dialect, false);
        return table
            .column(&n)
            .or_else(|| {
                table
                    .columns
                    .iter()
                    .find(|c| c.name.spelling().eq_ignore_ascii_case(name))
            })
            .map(|c| c.name.spelling().to_string())
            .ok_or_else(|| format!("display column `{name}` does not exist on the target table"));
    }
    let named = |candidate: &str| {
        table
            .columns
            .iter()
            .find(|c| c.name.spelling().eq_ignore_ascii_case(candidate))
            .map(|c| c.name.spelling().to_string())
    };
    let is_character_non_key = |c: &sqlly_lang::sqlmodel::ColumnDef| {
        !(key_col.matches(&c.name) || table.is_key_column(&c.name))
            && c.ty
                .as_ref()
                .is_some_and(|t| t.class == TypeClass::Character)
    };

    if let Some(found) = named("name") {
        return Ok(found);
    }
    for stem in table_name_stems(table_name) {
        for candidate in [format!("{stem}name"), format!("{stem}_name")] {
            if let Some(found) = named(&candidate) {
                return Ok(found);
            }
        }
    }
    if let Some(c) = table.columns.iter().find(|c| {
        is_character_non_key(c) && c.name.spelling().to_ascii_lowercase().ends_with("name")
    }) {
        return Ok(c.name.spelling().to_string());
    }
    for conventional in ["label", "display", "description", "code"] {
        if let Some(found) = named(conventional) {
            return Ok(found);
        }
    }
    if let Some(c) = table.columns.iter().find(|c| is_character_non_key(c)) {
        return Ok(c.name.spelling().to_string());
    }
    Ok(key_col.spelling().to_string())
}

/// The table name and, for a plural, its singular (`Categories` →
/// `Category`, `Boxes` → `Box`, `Orders` → `Order`), for `<Table>Name`.
fn table_name_stems(table_name: &str) -> Vec<String> {
    let mut stems = vec![table_name.to_string()];
    let lower = table_name.to_ascii_lowercase();
    let singular = if let Some(stem) = lower.strip_suffix("ies") {
        Some(format!("{}y", &table_name[..stem.len()]))
    } else if lower.ends_with("sses") || lower.ends_with("xes") || lower.ends_with("ches") {
        Some(table_name[..table_name.len() - 2].to_string())
    } else if lower.ends_with('s') && !lower.ends_with("ss") {
        Some(table_name[..table_name.len() - 1].to_string())
    } else {
        None
    };
    if let Some(singular) = singular.filter(|s| !s.is_empty()) {
        stems.push(singular);
    }
    stems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plural_table_names_yield_their_singular_stem() {
        assert_eq!(
            table_name_stems("Categories"),
            vec!["Categories", "Category"]
        );
        assert_eq!(table_name_stems("Products"), vec!["Products", "Product"]);
        assert_eq!(table_name_stems("Boxes"), vec!["Boxes", "Box"]);
        assert_eq!(table_name_stems("Addresses"), vec!["Addresses", "Address"]);
        assert_eq!(table_name_stems("Status"), vec!["Status", "Statu"]);
        assert_eq!(table_name_stems("Class"), vec!["Class"]);
        assert_eq!(table_name_stems("Region"), vec!["Region"]);
    }

    #[test]
    fn comment_delimiters_in_display_values_are_neutralized() {
        assert_eq!(fk_insert_text("1", "Beverages"), "1 /* Beverages */");
        assert_eq!(
            fk_insert_text("2", "a */ DROP TABLE t; /* b"),
            "2 /* a * / DROP TABLE t; / * b */"
        );
    }
}
