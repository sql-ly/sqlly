use super::{pg_ident, pg_literal};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoleOptions {
    pub login: bool,
    pub superuser: bool,
    pub createdb: bool,
    pub createrole: bool,
    pub inherit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TablePrivilege {
    Select,
    Insert,
    Update,
    Delete,
}

impl TablePrivilege {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }
}

pub fn roles_query() -> &'static str {
    "SELECT rolname, rolsuper, rolcreatedb, rolcreaterole, rolinherit, rolcanlogin, rolconnlimit, rolvaliduntil::text FROM pg_catalog.pg_roles WHERE rolname NOT LIKE 'pg\\_%' ORDER BY rolname"
}

pub fn role_members_query() -> &'static str {
    "SELECT r.rolname AS role, m.rolname AS member, am.admin_option FROM pg_catalog.pg_auth_members am JOIN pg_catalog.pg_roles r ON r.oid = am.roleid JOIN pg_catalog.pg_roles m ON m.oid = am.member ORDER BY 1, 2"
}

pub fn database_connect_grants_query() -> &'static str {
    "SELECT d.datname, r.rolname FROM pg_catalog.pg_database d CROSS JOIN pg_catalog.pg_roles r WHERE NOT d.datistemplate AND has_database_privilege(r.rolname, d.datname, 'CONNECT') AND NOT r.rolsuper AND r.rolname NOT LIKE 'pg\\_%' ORDER BY 1, 2"
}

pub fn schema_usage_query() -> &'static str {
    "SELECT n.nspname, r.rolname FROM pg_catalog.pg_namespace n CROSS JOIN pg_catalog.pg_roles r WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg\\_%' AND has_schema_privilege(r.rolname, n.nspname, 'USAGE') AND NOT r.rolsuper AND r.rolname NOT LIKE 'pg\\_%' ORDER BY 1, 2"
}

/// Table privileges held directly by one role (`schema, table, privilege`),
/// read from each relation's ACL (the owner's implicit privileges included)
/// and matched to the role by oid. information_schema spells the built-in
/// PUBLIC pseudo-role 'PUBLIC', exactly like a real role named "PUBLIC", so
/// matching by that name handed such a role PUBLIC's grants; PUBLIC is
/// grantee 0 in the ACL and has no `pg_roles` row.
pub fn table_grants_query(grantee: &str) -> String {
    format!(
        "SELECT DISTINCT n.nspname AS table_schema, c.relname AS table_name, a.privilege_type FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace CROSS JOIN LATERAL pg_catalog.aclexplode(COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner))) a JOIN pg_catalog.pg_roles r ON r.oid = a.grantee WHERE r.rolname = {} AND c.relkind IN ('r', 'v', 'm', 'f', 'p') AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_toast%' ORDER BY 1, 2, 3",
        pg_literal(grantee)
    )
}

pub fn tables_query() -> &'static str {
    "SELECT table_schema, table_name FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog','information_schema') ORDER BY 1, 2"
}

fn render_role_flags(opts: &RoleOptions) -> Vec<&'static str> {
    vec![
        if opts.login { "LOGIN" } else { "NOLOGIN" },
        if opts.superuser {
            "SUPERUSER"
        } else {
            "NOSUPERUSER"
        },
        if opts.createdb {
            "CREATEDB"
        } else {
            "NOCREATEDB"
        },
        if opts.createrole {
            "CREATEROLE"
        } else {
            "NOCREATEROLE"
        },
        if opts.inherit { "INHERIT" } else { "NOINHERIT" },
    ]
}

pub fn create_role_sql(name: &str, opts: &RoleOptions, password: Option<&str>) -> String {
    let mut sql = format!(
        "CREATE ROLE {} {}",
        pg_ident(name),
        render_role_flags(opts).join(" ")
    );
    if let Some(password) = password {
        sql.push_str(&format!(" PASSWORD {}", pg_literal(password)));
    }
    sql
}

pub fn alter_role_options_sql(name: &str, opts: &RoleOptions) -> String {
    format!(
        "ALTER ROLE {} {}",
        pg_ident(name),
        render_role_flags(opts).join(" ")
    )
}

pub fn alter_role_password_sql(name: &str, password: &str) -> String {
    format!(
        "ALTER ROLE {} PASSWORD {}",
        pg_ident(name),
        pg_literal(password)
    )
}

pub fn drop_role_sql(name: &str) -> String {
    format!("DROP ROLE {}", pg_ident(name))
}

pub fn grant_role_sql(role: &str, member: &str, admin: bool) -> String {
    let mut sql = format!("GRANT {} TO {}", pg_ident(role), pg_ident(member));
    if admin {
        sql.push_str(" WITH ADMIN OPTION");
    }
    sql
}

pub fn revoke_role_sql(role: &str, member: &str) -> String {
    format!("REVOKE {} FROM {}", pg_ident(role), pg_ident(member))
}

pub fn grant_connect_sql(db: &str, role: &str) -> String {
    format!(
        "GRANT CONNECT ON DATABASE {} TO {}",
        pg_ident(db),
        pg_ident(role)
    )
}

pub fn revoke_connect_sql(db: &str, role: &str) -> String {
    format!(
        "REVOKE CONNECT ON DATABASE {} FROM {}",
        pg_ident(db),
        pg_ident(role)
    )
}

pub fn grant_schema_usage_sql(schema: &str, role: &str) -> String {
    format!(
        "GRANT USAGE ON SCHEMA {} TO {}",
        pg_ident(schema),
        pg_ident(role)
    )
}

pub fn revoke_schema_usage_sql(schema: &str, role: &str) -> String {
    format!(
        "REVOKE USAGE ON SCHEMA {} FROM {}",
        pg_ident(schema),
        pg_ident(role)
    )
}

fn render_table_privileges(privs: &[TablePrivilege]) -> String {
    let mut ordered = privs.to_vec();
    ordered.sort();
    ordered
        .iter()
        .map(|p| p.as_sql())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn grant_table_sql(schema: &str, table: &str, privs: &[TablePrivilege], role: &str) -> String {
    format!(
        "GRANT {} ON TABLE {}.{} TO {}",
        render_table_privileges(privs),
        pg_ident(schema),
        pg_ident(table),
        pg_ident(role)
    )
}

pub fn revoke_table_sql(schema: &str, table: &str, privs: &[TablePrivilege], role: &str) -> String {
    format!(
        "REVOKE {} ON TABLE {}.{} FROM {}",
        render_table_privileges(privs),
        pg_ident(schema),
        pg_ident(table),
        pg_ident(role)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// information_schema spells the built-in PUBLIC pseudo-role 'PUBLIC',
    /// the same as a real role named "PUBLIC", so matching on that name gave
    /// such a role PUBLIC's grants. The grants come from the ACL itself,
    /// joined to the role by oid: PUBLIC (grantee 0) has no pg_roles row.
    #[test]
    fn postgres_table_grants_match_the_role_by_oid_not_the_public_spelling() {
        let sql = table_grants_query("PUBLIC");
        assert!(!sql.contains("role_table_grants"), "{sql}");
        assert!(sql.contains("aclexplode("), "{sql}");
        assert!(
            sql.contains("acldefault('r', c.relowner)"),
            "owners' implicit grants are kept: {sql}"
        );
        assert!(
            sql.contains("JOIN pg_catalog.pg_roles r ON r.oid = a.grantee"),
            "{sql}"
        );
        assert!(sql.contains("r.rolname = 'PUBLIC'"), "{sql}");
        assert!(
            table_grants_query("o'brien").contains("r.rolname = 'o''brien'"),
            "literal escaped"
        );
    }

    #[test]
    fn postgres_create_role_renders_all_flags_and_password() {
        let opts = RoleOptions {
            login: true,
            superuser: false,
            createdb: true,
            createrole: false,
            inherit: true,
        };
        let sql = create_role_sql("app", &opts, Some("secret"));
        assert_eq!(
            sql,
            "CREATE ROLE \"app\" LOGIN NOSUPERUSER CREATEDB NOCREATEROLE INHERIT PASSWORD 'secret'"
        );
    }

    #[test]
    fn postgres_identifiers_and_literals_are_escaped() {
        let opts = RoleOptions {
            login: true,
            ..RoleOptions::default()
        };
        let sql = create_role_sql("we\"ird", &opts, Some("it's"));
        assert_eq!(
            sql,
            "CREATE ROLE \"we\"\"ird\" LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT PASSWORD 'it''s'"
        );
    }

    #[test]
    fn postgres_grant_table_orders_privileges() {
        let privs = [
            TablePrivilege::Delete,
            TablePrivilege::Select,
            TablePrivilege::Update,
            TablePrivilege::Insert,
        ];
        let sql = grant_table_sql("public", "orders", &privs, "reader");
        assert_eq!(
            sql,
            "GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE \"public\".\"orders\" TO \"reader\""
        );
    }

    #[test]
    fn postgres_catalog_queries_are_stable() {
        assert_eq!(
            roles_query(),
            "SELECT rolname, rolsuper, rolcreatedb, rolcreaterole, rolinherit, rolcanlogin, rolconnlimit, rolvaliduntil::text FROM pg_catalog.pg_roles WHERE rolname NOT LIKE 'pg\\_%' ORDER BY rolname"
        );
    }
}
