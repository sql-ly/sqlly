//! PostgreSQL object privileges as the GRANT / REVOKE statements that
//! recreate them, computed the way `pg_dump` does.
//!
//! A new object's ACL is NULL, meaning "PostgreSQL's built-in defaults":
//! the owner holds every privilege, and for some kinds PUBLIC holds one too
//! (EXECUTE on functions and procedures, USAGE on types). Once anything is
//! granted or revoked the ACL is spelled out in full. Re-running only the
//! CREATE therefore hands back the defaults, including a PUBLIC EXECUTE an
//! administrator had revoked. The statements here diff the current ACL
//! against `acldefault(kind, owner)` through `aclexplode`: REVOKE what the
//! defaults grant but the ACL no longer does, GRANT what the ACL adds (WITH
//! GRANT OPTION when grantable). A NULL ACL produces no statements.
//!
//! Either side of that diff can be empty (a table with only grants, a
//! function with only its PUBLIC EXECUTE revoked, an ACL that was set but
//! equals the defaults). `ARRAY(SELECT … EXCEPT …)` then yields a
//! zero-dimensional empty array, which `aclexplode` rejects ("ACL arrays
//! must be one-dimensional"), so the query hands `aclexplode` NULL instead
//! of an empty array: the function is strict and NULL simply yields no rows.
//!
//! Column privileges (`pg_attribute.attacl`, whose default is empty) ride
//! along for tables and views as `GRANT SELECT(col) ON TABLE …`. A grant
//! made by someone other than the owner is replayed as that grantor, inside
//! `SET SESSION AUTHORIZATION` … `RESET SESSION AUTHORIZATION`, again as
//! `pg_dump` does, so the recreated ACL records the same grantor.
//!
//! Grantee 0 is the PUBLIC pseudo-role; every real role is written through
//! `regrole` output, which quotes as needed, so a real role named "PUBLIC"
//! comes out quoted and is never confused with PUBLIC.

use sqlly_core::{ConnectionProfile, StudioResult};

use crate::metadata::{cell_to_bool, cell_to_string, profile_with_db};
use crate::query::QueryExecutor;

/// The object whose privileges are scripted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgAclObject {
    /// A table, partitioned table, view, materialized view, foreign table
    /// (all `ON TABLE`, with their column privileges), or sequence
    /// (`ON SEQUENCE`).
    Relation {
        schema: String,
        name: String,
    },
    /// A function or procedure. When the name is overloaded, the overload
    /// with the lowest oid, the one the definition viewer shows.
    Routine {
        schema: String,
        name: String,
    },
    Schema {
        name: String,
    },
    /// A base type, enum, composite, range, or domain (`ON TYPE`).
    Type {
        schema: String,
        name: String,
    },
}

/// GRANT or REVOKE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegeAction {
    Revoke,
    Grant,
}

/// One exploded ACL difference: a single privilege for a single grantee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegeRow {
    pub action: PrivilegeAction,
    /// `SELECT`, `EXECUTE`, `USAGE`, …
    pub privilege: String,
    /// The grantee as SQL (already quoted when it needs to be); `None` is
    /// the PUBLIC pseudo-role.
    pub grantee: Option<String>,
    pub grantable: bool,
    /// The object as the statement names it: `TABLE "s"."t"`,
    /// `FUNCTION s.f(integer)`, …
    pub object: String,
    /// Who granted it, as SQL (quoted when needed); `None` when the owner
    /// did, which needs no `SET SESSION AUTHORIZATION`.
    pub grantor: Option<String>,
    /// For a column privilege, the column as SQL (quoted when needed).
    pub column: Option<String>,
}

fn lit(value: &str) -> String {
    value.replace('\'', "''")
}

/// The `(object_sql, acl, owner, kind, column_name, column_ord)` rows for
/// one object; `kind` is the `acldefault` object-type code. A relation
/// contributes one row for itself and one per column with its own ACL.
fn object_source_sql(object: &PgAclObject) -> String {
    match object {
        PgAclObject::Relation { schema, name } => format!(
            "(SELECT CASE c.relkind WHEN 'S' THEN 'SEQUENCE ' ELSE 'TABLE ' END || pg_catalog.quote_ident(n.nspname) || '.' || pg_catalog.quote_ident(c.relname) AS object_sql, c.relacl AS acl, c.relowner AS owner, CASE c.relkind WHEN 'S' THEN 's' ELSE 'r' END AS kind, NULL::text AS column_name, 0 AS column_ord FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = '{schema}' AND c.relname = '{name}' AND c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S') LIMIT 1) \
             UNION ALL \
             (SELECT 'TABLE ' || pg_catalog.quote_ident(n.nspname) || '.' || pg_catalog.quote_ident(c.relname), a.attacl, c.relowner, 'c', pg_catalog.quote_ident(a.attname), a.attnum::int FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = '{schema}' AND c.relname = '{name}' AND c.relkind IN ('r', 'p', 'v', 'm', 'f') AND a.attnum > 0 AND NOT a.attisdropped AND a.attacl IS NOT NULL)",
            schema = lit(schema),
            name = lit(name)
        ),
        PgAclObject::Routine { schema, name } => format!(
            "SELECT CASE p.prokind WHEN 'p' THEN 'PROCEDURE ' ELSE 'FUNCTION ' END || pg_catalog.quote_ident(n.nspname) || '.' || pg_catalog.quote_ident(p.proname) || '(' || pg_catalog.pg_get_function_identity_arguments(p.oid) || ')' AS object_sql, p.proacl AS acl, p.proowner AS owner, 'f' AS kind, NULL::text AS column_name, 0 AS column_ord FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = '{}' AND p.proname = '{}' AND p.prokind IN ('f', 'p') ORDER BY p.oid LIMIT 1",
            lit(schema),
            lit(name)
        ),
        PgAclObject::Schema { name } => format!(
            "SELECT 'SCHEMA ' || pg_catalog.quote_ident(n.nspname) AS object_sql, n.nspacl AS acl, n.nspowner AS owner, 'n' AS kind, NULL::text AS column_name, 0 AS column_ord FROM pg_catalog.pg_namespace n WHERE n.nspname = '{}'",
            lit(name)
        ),
        PgAclObject::Type { schema, name } => format!(
            "SELECT 'TYPE ' || pg_catalog.quote_ident(n.nspname) || '.' || pg_catalog.quote_ident(t.typname) AS object_sql, t.typacl AS acl, t.typowner AS owner, 'T' AS kind, NULL::text AS column_name, 0 AS column_ord FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace WHERE n.nspname = '{}' AND t.typname = '{}' LIMIT 1",
            lit(schema),
            lit(name)
        ),
    }
}

/// The query listing the object's privilege differences from PostgreSQL's
/// defaults: `(action, privilege_type, grantee, is_grantable, object_sql,
/// grantor, column_name)`, REVOKEs first, then GRANTs by the owner, then
/// GRANTs by each other grantor; a relation's own privileges come before
/// its columns'. `grantee` is NULL for PUBLIC, `grantor` is NULL when it is
/// the owner, `column_name` is NULL for the object itself.
pub fn privilege_diff_sql(object: &PgAclObject) -> String {
    format!(
        "SELECT d.action, x.privilege_type, CASE WHEN x.grantee = 0 THEN NULL ELSE x.grantee::pg_catalog.regrole::text END AS grantee, x.is_grantable, o.object_sql, CASE WHEN x.grantor = o.owner THEN NULL ELSE x.grantor::pg_catalog.regrole::text END AS grantor, o.column_name \
         FROM ({source}) o \
         CROSS JOIN LATERAL (SELECT 'REVOKE' AS action, 1 AS ord, ARRAY(SELECT pg_catalog.unnest(pg_catalog.acldefault(o.kind::\"char\", o.owner)) EXCEPT SELECT pg_catalog.unnest(o.acl)) AS items \
           UNION ALL SELECT 'GRANT', 2, ARRAY(SELECT pg_catalog.unnest(o.acl) EXCEPT SELECT pg_catalog.unnest(pg_catalog.acldefault(o.kind::\"char\", o.owner)))) d \
         CROSS JOIN LATERAL pg_catalog.aclexplode(CASE WHEN pg_catalog.cardinality(d.items) = 0 THEN NULL ELSE d.items END) x \
         WHERE o.acl IS NOT NULL \
         ORDER BY d.ord, (x.grantor <> o.owner), grantor, o.column_ord, (x.grantee <> 0), grantee, x.is_grantable, \
           pg_catalog.array_position(ARRAY['SELECT', 'INSERT', 'UPDATE', 'DELETE', 'TRUNCATE', 'REFERENCES', 'TRIGGER', 'MAINTAIN', 'USAGE', 'CREATE', 'EXECUTE'], x.privilege_type::text), x.privilege_type",
        source = object_source_sql(object)
    )
}

/// Statements in the query's order, one per (action, grantee, grant option,
/// column) run, privileges comma-joined: `REVOKE EXECUTE ON FUNCTION s.f()
/// FROM PUBLIC;`, `GRANT SELECT, INSERT ON TABLE s.t TO app WITH GRANT
/// OPTION;`, `GRANT SELECT(secret) ON TABLE s.t TO app;`. Consecutive
/// statements made by a grantor other than the owner are wrapped in `SET
/// SESSION AUTHORIZATION grantor;` … `RESET SESSION AUTHORIZATION;`.
pub fn render_privilege_statements(rows: &[PrivilegeRow]) -> Vec<String> {
    let mut out = Vec::new();
    let mut current_grantor: Option<&str> = None;
    let mut i = 0;
    while i < rows.len() {
        let head = &rows[i];
        let mut privileges = vec![head.privilege.as_str()];
        let mut j = i + 1;
        while j < rows.len()
            && rows[j].action == head.action
            && rows[j].grantee == head.grantee
            && rows[j].grantable == head.grantable
            && rows[j].object == head.object
            && rows[j].grantor == head.grantor
            && rows[j].column == head.column
        {
            if !privileges.contains(&rows[j].privilege.as_str()) {
                privileges.push(rows[j].privilege.as_str());
            }
            j += 1;
        }
        if head.grantor.as_deref() != current_grantor {
            if current_grantor.is_some() {
                out.push("RESET SESSION AUTHORIZATION;".to_string());
            }
            if let Some(grantor) = &head.grantor {
                out.push(format!("SET SESSION AUTHORIZATION {grantor};"));
            }
            current_grantor = head.grantor.as_deref();
        }
        let grantee = head.grantee.as_deref().unwrap_or("PUBLIC");
        let privileges = match &head.column {
            Some(column) => privileges
                .iter()
                .map(|p| format!("{p}({column})"))
                .collect::<Vec<_>>()
                .join(", "),
            None => privileges.join(", "),
        };
        out.push(match head.action {
            PrivilegeAction::Revoke => {
                format!("REVOKE {privileges} ON {} FROM {grantee};", head.object)
            }
            PrivilegeAction::Grant => format!(
                "GRANT {privileges} ON {} TO {grantee}{};",
                head.object,
                if head.grantable {
                    " WITH GRANT OPTION"
                } else {
                    ""
                }
            ),
        });
        i = j;
    }
    if current_grantor.is_some() {
        out.push("RESET SESSION AUTHORIZATION;".to_string());
    }
    out
}

/// Read and render the object's GRANT / REVOKE statements. Empty when the
/// object has PostgreSQL's default privileges (a NULL ACL) or its ACL
/// matches them.
pub async fn fetch_privilege_statements(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    database: &str,
    object: &PgAclObject,
) -> StudioResult<Vec<String>> {
    let rows = crate::ddl::collect_rows(
        queries,
        profile_with_db(profile, database),
        &privilege_diff_sql(object),
    )
    .await?;
    let rows: Vec<PrivilegeRow> = rows
        .into_iter()
        .filter_map(|row| {
            let mut cells = row.into_iter();
            let action = match cell_to_string(cells.next())?.as_str() {
                "REVOKE" => PrivilegeAction::Revoke,
                "GRANT" => PrivilegeAction::Grant,
                _ => return None,
            };
            let privilege = cell_to_string(cells.next())?;
            let grantee = cell_to_string(cells.next()).filter(|g| !g.is_empty());
            let grantable = cell_to_bool(cells.next()).unwrap_or(false);
            let object = cell_to_string(cells.next())?;
            let grantor = cell_to_string(cells.next()).filter(|g| !g.is_empty());
            let column = cell_to_string(cells.next()).filter(|c| !c.is_empty());
            Some(PrivilegeRow {
                action,
                privilege,
                grantee,
                grantable,
                object,
                grantor,
                column,
            })
        })
        .collect();
    Ok(render_privilege_statements(&rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        action: PrivilegeAction,
        privilege: &str,
        grantee: Option<&str>,
        grantable: bool,
        object: &str,
    ) -> PrivilegeRow {
        PrivilegeRow {
            action,
            privilege: privilege.into(),
            grantee: grantee.map(str::to_string),
            grantable,
            object: object.into(),
            grantor: None,
            column: None,
        }
    }

    #[test]
    fn revoked_public_execute_and_a_grant_render_like_pg_dump() {
        let f = "FUNCTION public.acltest()";
        let rows = vec![
            row(PrivilegeAction::Revoke, "EXECUTE", None, false, f),
            row(
                PrivilegeAction::Grant,
                "EXECUTE",
                Some("app_reader"),
                false,
                f,
            ),
        ];
        assert_eq!(
            render_privilege_statements(&rows),
            vec![
                "REVOKE EXECUTE ON FUNCTION public.acltest() FROM PUBLIC;".to_string(),
                "GRANT EXECUTE ON FUNCTION public.acltest() TO app_reader;".to_string(),
            ]
        );
    }

    #[test]
    fn privileges_for_one_grantee_combine_and_grant_option_splits() {
        let t = "TABLE public.orders";
        let rows = vec![
            row(PrivilegeAction::Revoke, "TRUNCATE", Some("owner"), false, t),
            row(PrivilegeAction::Grant, "SELECT", Some("app"), false, t),
            row(PrivilegeAction::Grant, "INSERT", Some("app"), false, t),
            row(PrivilegeAction::Grant, "UPDATE", Some("app"), true, t),
            row(
                PrivilegeAction::Grant,
                "SELECT",
                Some("\"PUBLIC\""),
                false,
                t,
            ),
            row(PrivilegeAction::Grant, "SELECT", None, false, t),
        ];
        assert_eq!(
            render_privilege_statements(&rows),
            vec![
                "REVOKE TRUNCATE ON TABLE public.orders FROM owner;".to_string(),
                "GRANT SELECT, INSERT ON TABLE public.orders TO app;".to_string(),
                "GRANT UPDATE ON TABLE public.orders TO app WITH GRANT OPTION;".to_string(),
                "GRANT SELECT ON TABLE public.orders TO \"PUBLIC\";".to_string(),
                "GRANT SELECT ON TABLE public.orders TO PUBLIC;".to_string(),
            ]
        );
    }

    #[test]
    fn column_privileges_name_the_column_and_split_per_column() {
        let t = "TABLE public.people";
        let col = |privilege: &str, column: &str| PrivilegeRow {
            column: Some(column.into()),
            ..row(PrivilegeAction::Grant, privilege, Some("app"), false, t)
        };
        let rows = vec![
            row(PrivilegeAction::Grant, "SELECT", Some("app"), false, t),
            col("SELECT", "secret"),
            col("UPDATE", "secret"),
            col("SELECT", "\"Email\""),
        ];
        assert_eq!(
            render_privilege_statements(&rows),
            vec![
                "GRANT SELECT ON TABLE public.people TO app;".to_string(),
                "GRANT SELECT(secret), UPDATE(secret) ON TABLE public.people TO app;".to_string(),
                "GRANT SELECT(\"Email\") ON TABLE public.people TO app;".to_string(),
            ]
        );
    }

    #[test]
    fn grants_by_another_grantor_run_as_that_grantor() {
        let t = "TABLE public.orders";
        let by = |grantor: &str, grantee: &str, privilege: &str| PrivilegeRow {
            grantor: Some(grantor.into()),
            ..row(PrivilegeAction::Grant, privilege, Some(grantee), false, t)
        };
        let rows = vec![
            row(PrivilegeAction::Grant, "SELECT", Some("lead"), true, t),
            by("lead", "app", "SELECT"),
            by("lead", "app", "INSERT"),
            by("lead", "reporting", "SELECT"),
            by("\"Audit Lead\"", "auditor", "SELECT"),
        ];
        assert_eq!(
            render_privilege_statements(&rows),
            vec![
                "GRANT SELECT ON TABLE public.orders TO lead WITH GRANT OPTION;".to_string(),
                "SET SESSION AUTHORIZATION lead;".to_string(),
                "GRANT SELECT, INSERT ON TABLE public.orders TO app;".to_string(),
                "GRANT SELECT ON TABLE public.orders TO reporting;".to_string(),
                "RESET SESSION AUTHORIZATION;".to_string(),
                "SET SESSION AUTHORIZATION \"Audit Lead\";".to_string(),
                "GRANT SELECT ON TABLE public.orders TO auditor;".to_string(),
                "RESET SESSION AUTHORIZATION;".to_string(),
            ]
        );
        // A grantor block in the middle is closed before the owner's next
        // statement; nothing dangles.
        let rows = vec![
            by("lead", "app", "SELECT"),
            row(PrivilegeAction::Grant, "SELECT", Some("other"), false, t),
        ];
        assert_eq!(
            render_privilege_statements(&rows),
            vec![
                "SET SESSION AUTHORIZATION lead;".to_string(),
                "GRANT SELECT ON TABLE public.orders TO app;".to_string(),
                "RESET SESSION AUTHORIZATION;".to_string(),
                "GRANT SELECT ON TABLE public.orders TO other;".to_string(),
            ]
        );
    }

    #[test]
    fn no_differences_render_nothing() {
        assert!(render_privilege_statements(&[]).is_empty());
    }

    #[test]
    fn diff_query_compares_the_acl_with_the_kind_default() {
        let objects = [
            (
                PgAclObject::Relation {
                    schema: "public".into(),
                    name: "o'rders".into(),
                },
                "c.relacl",
            ),
            (
                PgAclObject::Routine {
                    schema: "public".into(),
                    name: "f".into(),
                },
                "p.proacl",
            ),
            (PgAclObject::Schema { name: "app".into() }, "n.nspacl"),
            (
                PgAclObject::Type {
                    schema: "public".into(),
                    name: "mood".into(),
                },
                "t.typacl",
            ),
        ];
        for (object, acl) in objects {
            let sql = privilege_diff_sql(&object);
            assert!(sql.contains(acl), "{sql}");
            // An empty side of the diff is a zero-dimensional array that
            // aclexplode rejects; it must be handed NULL instead.
            assert!(
                sql.contains(
                    "pg_catalog.aclexplode(CASE WHEN pg_catalog.cardinality(d.items) = 0 THEN NULL ELSE d.items END)"
                ),
                "{sql}"
            );
            assert!(!sql.contains("aclexplode(d.items)"), "{sql}");
            assert!(
                sql.contains("pg_catalog.acldefault(o.kind::\"char\", o.owner)"),
                "{sql}"
            );
            assert!(sql.contains("WHERE o.acl IS NOT NULL"), "{sql}");
            assert!(sql.contains("WHEN x.grantee = 0 THEN NULL"), "{sql}");
            assert!(sql.contains("WHEN x.grantor = o.owner THEN NULL"), "{sql}");
            assert!(sql.contains("regrole"), "{sql}");
            assert!(sql.contains("o.column_name"), "{sql}");
            assert!(sql.contains("(x.grantor <> o.owner), grantor"), "{sql}");
        }
        let sql = privilege_diff_sql(&PgAclObject::Relation {
            schema: "public".into(),
            name: "o'rders".into(),
        });
        assert!(sql.contains("c.relname = 'o''rders'"), "{sql}");
        assert!(sql.contains("'SEQUENCE '"), "{sql}");
        // Column privileges: only columns with an ACL of their own, as the
        // column-kind default ('c', which is empty).
        assert!(sql.contains("a.attacl IS NOT NULL"), "{sql}");
        assert!(sql.contains("NOT a.attisdropped"), "{sql}");
        assert!(sql.contains("pg_catalog.quote_ident(a.attname)"), "{sql}");
        assert!(sql.contains(", 'c', "), "{sql}");
        let sql = privilege_diff_sql(&PgAclObject::Routine {
            schema: "public".into(),
            name: "f".into(),
        });
        assert!(sql.contains("'PROCEDURE '"), "{sql}");
        assert!(sql.contains("pg_get_function_identity_arguments"), "{sql}");
        assert!(sql.contains("ORDER BY p.oid LIMIT 1"), "{sql}");
        assert!(!sql.contains("pg_attribute"), "{sql}");
    }
}
