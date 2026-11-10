//! Oracle object-dependency queries for View Dependencies / Find Usages:
//! `ALL_DEPENDENCIES` in both directions (what the object uses, what uses
//! it) plus foreign keys from `ALL_CONSTRAINTS`, and a textual search of
//! stored PL/SQL source (`ALL_SOURCE`) and view text (`ALL_VIEWS.TEXT_VC`,
//! 12c+). Pure SQL construction; the GPUI dialog dispatches here for
//! `ConnectionKind::OracleNet`. Names arrive as the explorer stores them
//! (upper case for unquoted objects) and are literal-escaped, never
//! interpolated raw.

/// Double embedded single quotes so `s` is inert inside a `'…'` literal.
fn literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// Owner literal: an empty schema means the session's current schema.
fn owner_value(schema: &str) -> String {
    if schema.is_empty() {
        "SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')".to_string()
    } else {
        format!("'{}'", literal(schema))
    }
}

/// LIKE-pattern fragment: upper-cased (stored source is matched
/// case-insensitively), `\`-escaped `% _` paired with `ESCAPE '\'`, then
/// literal-escaped.
fn like_fragment(name: &str) -> String {
    literal(
        &name
            .to_uppercase()
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_"),
    )
}

/// Dependency listing for `schema.name`, three columns: Direction
/// (`Uses` / `Used by`), Object (owner-qualified), Type.
pub fn dependencies_sql(schema: &str, name: &str) -> String {
    let owner = owner_value(schema);
    let name = literal(name);
    format!(
        r#"SELECT "Direction", "Object", "Type" FROM (
  SELECT DISTINCT 'Used by' AS "Direction",
         d.owner || '.' || d.name AS "Object",
         d.type AS "Type"
  FROM all_dependencies d
  WHERE d.referenced_owner = {owner} AND d.referenced_name = '{name}'
  UNION
  SELECT DISTINCT 'Used by',
         c.owner || '.' || c.table_name,
         'FOREIGN KEY ' || c.constraint_name
  FROM all_constraints c
  JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name
  WHERE c.constraint_type = 'R' AND r.owner = {owner} AND r.table_name = '{name}'
  UNION
  SELECT DISTINCT 'Uses',
         d2.referenced_owner || '.' || d2.referenced_name,
         d2.referenced_type
  FROM all_dependencies d2
  WHERE d2.owner = {owner} AND d2.name = '{name}'
  UNION
  SELECT DISTINCT 'Uses',
         r2.owner || '.' || r2.table_name,
         'FOREIGN KEY ' || c2.constraint_name
  FROM all_constraints c2
  JOIN all_constraints r2 ON r2.owner = c2.r_owner AND r2.constraint_name = c2.r_constraint_name
  WHERE c2.constraint_type = 'R' AND c2.owner = {owner} AND c2.table_name = '{name}'
) ORDER BY "Direction", "Object""#
    )
}

/// Textual search for `name` across stored PL/SQL units and view text,
/// three columns: Object, Type, Preview (the first matching line, 200
/// chars), capped at `limit` rows. The object itself and the SYS / SYSTEM
/// schemas are excluded.
pub fn usages_sql(schema: &str, name: &str, limit: usize) -> String {
    let owner = owner_value(schema);
    let self_name = literal(name);
    let pattern = like_fragment(name);
    format!(
        r#"SELECT "Object", "Type", "Preview" FROM (
  SELECT s.owner || '.' || s.name AS "Object",
         s.type AS "Type",
         MIN(SUBSTR(s.text, 1, 200)) KEEP (DENSE_RANK FIRST ORDER BY s.line) AS "Preview"
  FROM all_source s
  WHERE UPPER(s.text) LIKE '%{pattern}%' ESCAPE '\'
    AND NOT (s.owner = {owner} AND s.name = '{self_name}')
    AND s.owner NOT IN ('SYS', 'SYSTEM')
  GROUP BY s.owner, s.name, s.type
  UNION ALL
  SELECT v.owner || '.' || v.view_name,
         'VIEW',
         SUBSTR(v.text_vc, 1, 200)
  FROM all_views v
  WHERE UPPER(v.text_vc) LIKE '%{pattern}%' ESCAPE '\'
    AND NOT (v.owner = {owner} AND v.view_name = '{self_name}')
    AND v.owner NOT IN ('SYS', 'SYSTEM')
) ORDER BY "Object" FETCH FIRST {limit} ROWS ONLY"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependencies_read_all_dependencies_both_ways_and_foreign_keys() {
        let sql = dependencies_sql("HR", "EMP");
        assert!(sql.contains("FROM all_dependencies d"));
        assert!(sql.contains("d.referenced_owner = 'HR' AND d.referenced_name = 'EMP'"));
        assert!(sql.contains("d2.owner = 'HR' AND d2.name = 'EMP'"));
        assert!(sql.contains("'FOREIGN KEY ' || c.constraint_name"));
        assert!(sql.contains("c2.constraint_type = 'R'"));
        assert!(sql.ends_with("ORDER BY \"Direction\", \"Object\""));
    }

    #[test]
    fn empty_schema_means_the_current_schema_and_names_are_escaped() {
        let sql = dependencies_sql("", "O'HARA");
        assert!(sql.contains("SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')"));
        assert!(sql.contains("'O''HARA'"));
        assert!(!sql.contains("'O'HARA'"));
    }

    #[test]
    fn usages_search_source_and_view_text_case_insensitively() {
        let sql = usages_sql("HR", "emp_v1", 500);
        assert!(sql.contains("FROM all_source s"));
        assert!(sql.contains("LIKE '%EMP\\_V1%' ESCAPE '\\'"), "{sql}");
        assert!(sql.contains("v.text_vc"));
        assert!(sql.contains("NOT (s.owner = 'HR' AND s.name = 'emp_v1')"));
        assert!(sql.contains("s.owner NOT IN ('SYS', 'SYSTEM')"));
        assert!(sql.ends_with("FETCH FIRST 500 ROWS ONLY"));
        let hostile = usages_sql("", "a%b'c", 10);
        assert!(hostile.contains("'%A\\%B''C%'"), "{hostile}");
    }

    #[test]
    fn queries_are_read_only() {
        for sql in [dependencies_sql("S", "T"), usages_sql("S", "T", 10)] {
            let upper = sql.to_ascii_uppercase();
            for verb in [
                "INSERT ", "UPDATE ", "DELETE ", "DROP ", "ALTER ", "CREATE ",
            ] {
                assert!(!upper.contains(verb), "{verb} in {sql}");
            }
        }
    }
}
