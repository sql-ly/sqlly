//! Oracle users, roles and grants: catalog queries over the `DBA_*` views
//! (with `USER_*` / `ALL_*` fallbacks for logins without dictionary access)
//! and the DDL the Security editor scripts.
//!
//! The catalog queries deliberately return the same column shapes as the
//! PostgreSQL ones, so the Security dialog's Roles tab drives both engines:
//! an Oracle *user* is a role that can log in, an Oracle *role* one that
//! cannot, `CREATE SESSION` plays the part of database CONNECT, and the DBA
//! role stands in for "superuser". Oracle-maintained accounts (SYS, SYSTEM,
//! the `*$` internals) are filtered out so the list shows what an
//! administrator actually manages.

use super::postgres::TablePrivilege;

/// Double-quote an Oracle identifier exactly as given, escaping embedded `"`.
pub(crate) fn oracle_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Single-quote an Oracle string literal, escaping embedded `'`.
pub(crate) fn oracle_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The quoted spelling of a name the user just typed for a NEW user or
/// role: a plain identifier (letters, digits, `_`, `$`, `#`, not starting
/// with a digit) is upper-cased the way Oracle would store it unquoted, so
/// the account can log in without quoting; anything else is quoted as-is.
pub fn oracle_new_name(s: &str) -> String {
    let trimmed = s.trim();
    let plain = !trimmed.is_empty()
        && !trimmed.starts_with(|c: char| c.is_ascii_digit())
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#'));
    if plain {
        oracle_ident(&trimmed.to_ascii_uppercase())
    } else {
        oracle_ident(trimmed)
    }
}

/// Grantees an administrator manages: every non-Oracle-maintained user and
/// role. Used as an `IN (…)` filter by the grant queries.
const MANAGED_GRANTEES: &str = "SELECT username FROM dba_users WHERE oracle_maintained = 'N' UNION SELECT role FROM dba_roles WHERE oracle_maintained = 'N'";

// ── Catalog: PostgreSQL-shaped sets for the Roles tab ────────────────────

/// Users and roles as one list (`name, superuser, createdb, createrole,
/// inherit, canlogin, connlimit, valid_until`): users can log in, roles
/// cannot; "superuser" means the DBA role is granted.
pub fn roles_query() -> &'static str {
    "SELECT p.name, p.is_dba AS rolsuper, 0 AS rolcreatedb, p.createrole AS rolcreaterole, 1 AS rolinherit, p.canlogin AS rolcanlogin, -1 AS rolconnlimit, p.valid_until AS rolvaliduntil FROM (SELECT u.username AS name, CASE WHEN EXISTS (SELECT 1 FROM dba_role_privs rp WHERE rp.grantee = u.username AND rp.granted_role = 'DBA') THEN 1 ELSE 0 END AS is_dba, CASE WHEN EXISTS (SELECT 1 FROM dba_sys_privs sp WHERE sp.grantee = u.username AND sp.privilege = 'CREATE ROLE') THEN 1 ELSE 0 END AS createrole, 1 AS canlogin, TO_CHAR(u.expiry_date, 'YYYY-MM-DD HH24:MI:SS') AS valid_until FROM dba_users u WHERE u.oracle_maintained = 'N' UNION ALL SELECT r.role, 0, 0, 0, CAST(NULL AS VARCHAR2(19)) FROM dba_roles r WHERE r.oracle_maintained = 'N') p ORDER BY p.canlogin DESC, p.name"
}

/// [`roles_query`] without dictionary access: the session user (with its
/// own DBA / CREATE ROLE facts) plus every other visible user; roles are
/// not visible without `DBA_ROLES`.
pub fn roles_query_compat() -> &'static str {
    "SELECT p.name, p.is_dba AS rolsuper, 0 AS rolcreatedb, p.createrole AS rolcreaterole, 1 AS rolinherit, 1 AS rolcanlogin, -1 AS rolconnlimit, CAST(NULL AS VARCHAR2(19)) AS rolvaliduntil FROM (SELECT USER AS name, CASE WHEN EXISTS (SELECT 1 FROM user_role_privs rp WHERE rp.granted_role = 'DBA') THEN 1 ELSE 0 END AS is_dba, CASE WHEN EXISTS (SELECT 1 FROM user_sys_privs sp WHERE sp.privilege = 'CREATE ROLE') THEN 1 ELSE 0 END AS createrole FROM DUAL UNION ALL SELECT u.username, 0, 0 FROM all_users u WHERE u.username <> USER AND u.oracle_maintained = 'N') p ORDER BY p.name"
}

/// `(role, member, admin_option)` for every managed grantee.
pub fn role_members_query() -> String {
    format!(
        "SELECT rp.granted_role AS role, rp.grantee AS member, CASE rp.admin_option WHEN 'YES' THEN 1 ELSE 0 END AS admin_option FROM dba_role_privs rp WHERE rp.grantee IN ({MANAGED_GRANTEES}) AND rp.granted_role IN (SELECT role FROM dba_roles WHERE oracle_maintained = 'N' UNION SELECT 'DBA' FROM DUAL UNION SELECT 'CONNECT' FROM DUAL UNION SELECT 'RESOURCE' FROM DUAL) ORDER BY 1, 2"
    )
}

/// [`role_members_query`] for the session user only.
pub fn role_members_query_compat() -> &'static str {
    "SELECT rp.granted_role AS role, rp.username AS member, CASE rp.admin_option WHEN 'YES' THEN 1 ELSE 0 END AS admin_option FROM user_role_privs rp ORDER BY 1, 2"
}

/// `(container, grantee)` for every managed grantee that can open a session
/// (`CREATE SESSION` directly or through the CONNECT role). The container
/// itself is listed once against PUBLIC so the dialog always offers it.
pub fn database_connect_grants_query() -> String {
    format!(
        "SELECT SYS_CONTEXT('USERENV', 'CON_NAME') AS datname, g.grantee AS rolname FROM (SELECT sp.grantee FROM dba_sys_privs sp WHERE sp.privilege = 'CREATE SESSION' UNION SELECT rp.grantee FROM dba_role_privs rp WHERE rp.granted_role = 'CONNECT') g WHERE g.grantee IN ({MANAGED_GRANTEES}) UNION ALL SELECT SYS_CONTEXT('USERENV', 'CON_NAME'), 'PUBLIC' FROM DUAL ORDER BY 1, 2"
    )
}

/// [`database_connect_grants_query`] for the session user only.
pub fn database_connect_grants_query_compat() -> &'static str {
    "SELECT SYS_CONTEXT('USERENV', 'CON_NAME') AS datname, USER AS rolname FROM DUAL WHERE EXISTS (SELECT 1 FROM user_sys_privs WHERE privilege = 'CREATE SESSION') OR EXISTS (SELECT 1 FROM user_role_privs WHERE granted_role = 'CONNECT') UNION ALL SELECT SYS_CONTEXT('USERENV', 'CON_NAME'), 'PUBLIC' FROM DUAL ORDER BY 1, 2"
}

/// Oracle has no schema USAGE privilege: an empty `(schema, grantee)` set in
/// the PostgreSQL shape, so the shared Roles tab shows nothing to tick.
pub fn schema_usage_query() -> &'static str {
    "SELECT CAST(NULL AS VARCHAR2(128)) AS nspname, CAST(NULL AS VARCHAR2(128)) AS rolname FROM DUAL WHERE 1 = 0"
}

/// `(owner, table)` for every table outside the Oracle-maintained schemas.
pub fn tables_query() -> &'static str {
    "SELECT t.owner AS table_schema, t.table_name FROM all_tables t WHERE t.owner NOT IN (SELECT username FROM all_users WHERE oracle_maintained = 'Y') AND t.table_name NOT LIKE 'BIN$%' ORDER BY 1, 2"
}

/// Table privileges held directly by one grantee (`schema, table, privilege`).
pub fn table_grants_query(grantee: &str) -> String {
    format!(
        "SELECT tp.owner AS table_schema, tp.table_name, tp.privilege AS privilege_type FROM dba_tab_privs tp WHERE tp.grantee = {} AND tp.type = 'TABLE' ORDER BY 1, 2, 3",
        oracle_literal(grantee)
    )
}

/// [`table_grants_query`] through `ALL_TAB_PRIVS` (grants the session user
/// made, received, or owns).
pub fn table_grants_query_compat(grantee: &str) -> String {
    format!(
        "SELECT tp.table_schema, tp.table_name, tp.privilege AS privilege_type FROM all_tab_privs tp WHERE tp.grantee = {} AND tp.type = 'TABLE' ORDER BY 1, 2, 3",
        oracle_literal(grantee)
    )
}

// ── Catalog: Oracle-only detail sets ─────────────────────────────────────

/// Every managed user with its account state.
pub fn users_query() -> &'static str {
    "SELECT u.username, u.account_status, TO_CHAR(u.lock_date, 'YYYY-MM-DD HH24:MI:SS') AS lock_date, TO_CHAR(u.expiry_date, 'YYYY-MM-DD HH24:MI:SS') AS expiry_date, u.default_tablespace, u.profile, TO_CHAR(u.created, 'YYYY-MM-DD HH24:MI:SS') AS created FROM dba_users u WHERE u.oracle_maintained = 'N' ORDER BY u.username"
}

/// [`users_query`] through `ALL_USERS` (names and creation dates only).
pub fn users_query_compat() -> &'static str {
    "SELECT u.username, CAST(NULL AS VARCHAR2(32)) AS account_status, CAST(NULL AS VARCHAR2(19)) AS lock_date, CAST(NULL AS VARCHAR2(19)) AS expiry_date, CAST(NULL AS VARCHAR2(30)) AS default_tablespace, CAST(NULL AS VARCHAR2(128)) AS profile, TO_CHAR(u.created, 'YYYY-MM-DD HH24:MI:SS') AS created FROM all_users u WHERE u.oracle_maintained = 'N' ORDER BY u.username"
}

/// Every managed role.
pub fn dba_roles_query() -> &'static str {
    "SELECT r.role, r.authentication_type, r.common FROM dba_roles r WHERE r.oracle_maintained = 'N' ORDER BY r.role"
}

/// System privileges held directly by one grantee.
pub fn system_privileges_query(grantee: &str) -> String {
    format!(
        "SELECT sp.privilege, CASE sp.admin_option WHEN 'YES' THEN 1 ELSE 0 END AS admin_option FROM dba_sys_privs sp WHERE sp.grantee = {} ORDER BY sp.privilege",
        oracle_literal(grantee)
    )
}

/// System privileges of the session user.
pub fn system_privileges_query_compat() -> &'static str {
    "SELECT sp.privilege, CASE sp.admin_option WHEN 'YES' THEN 1 ELSE 0 END AS admin_option FROM user_sys_privs sp ORDER BY sp.privilege"
}

/// Profiles with their password and session limits.
pub fn profiles_query() -> &'static str {
    "SELECT p.profile, p.resource_name, p.resource_type, p.limit FROM dba_profiles p ORDER BY p.profile, p.resource_type, p.resource_name"
}

// ── Scripting ────────────────────────────────────────────────────────────

/// `CREATE USER … IDENTIFIED BY …` with optional tablespace and profile.
/// The password is double-quoted so it may carry punctuation; it cannot
/// contain `"` itself.
pub fn create_user_sql(
    name: &str,
    password: &str,
    default_tablespace: Option<&str>,
    profile: Option<&str>,
) -> String {
    let mut sql = format!(
        "CREATE USER {} IDENTIFIED BY {}",
        oracle_new_name(name),
        password_literal(password)
    );
    if let Some(ts) = default_tablespace.filter(|s| !s.trim().is_empty()) {
        sql.push_str(&format!(" DEFAULT TABLESPACE {}", oracle_ident(ts.trim())));
    }
    if let Some(p) = profile.filter(|s| !s.trim().is_empty()) {
        sql.push_str(&format!(" PROFILE {}", oracle_ident(p.trim())));
    }
    sql.push(';');
    sql
}

fn password_literal(password: &str) -> String {
    format!("\"{}\"", password.replace('"', ""))
}

pub fn create_role_sql(name: &str) -> String {
    format!("CREATE ROLE {};", oracle_new_name(name))
}

pub fn grant_role_sql(role: &str, grantee: &str, admin: bool) -> String {
    let mut sql = format!("GRANT {} TO {}", oracle_ident(role), oracle_ident(grantee));
    if admin {
        sql.push_str(" WITH ADMIN OPTION");
    }
    sql.push(';');
    sql
}

pub fn revoke_role_sql(role: &str, grantee: &str) -> String {
    format!(
        "REVOKE {} FROM {};",
        oracle_ident(role),
        oracle_ident(grantee)
    )
}

/// `GRANT <system privilege> TO …` (`CREATE SESSION`, `CREATE ROLE`, …).
/// The privilege is a keyword phrase, never quoted; only known spellings
/// (letters and spaces) are accepted so nothing else can be smuggled in.
pub fn grant_system_privilege_sql(privilege: &str, grantee: &str) -> String {
    format!(
        "GRANT {} TO {};",
        system_privilege(privilege),
        oracle_ident(grantee)
    )
}

pub fn revoke_system_privilege_sql(privilege: &str, grantee: &str) -> String {
    format!(
        "REVOKE {} FROM {};",
        system_privilege(privilege),
        oracle_ident(grantee)
    )
}

fn system_privilege(privilege: &str) -> String {
    privilege
        .split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|c| c.is_ascii_alphabetic() || *c == '_')
                .collect::<String>()
                .to_ascii_uppercase()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn grant_create_session_sql(grantee: &str) -> String {
    grant_system_privilege_sql("CREATE SESSION", grantee)
}

pub fn revoke_create_session_sql(grantee: &str) -> String {
    revoke_system_privilege_sql("CREATE SESSION", grantee)
}

fn privilege_list(privs: &[TablePrivilege]) -> String {
    let mut sorted: Vec<TablePrivilege> = privs.to_vec();
    sorted.sort();
    sorted.dedup();
    sorted
        .iter()
        .map(|p| p.as_sql())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn grant_table_sql(
    schema: &str,
    table: &str,
    privs: &[TablePrivilege],
    grantee: &str,
) -> String {
    format!(
        "GRANT {} ON {}.{} TO {};",
        privilege_list(privs),
        oracle_ident(schema),
        oracle_ident(table),
        oracle_ident(grantee)
    )
}

pub fn revoke_table_sql(
    schema: &str,
    table: &str,
    privs: &[TablePrivilege],
    grantee: &str,
) -> String {
    format!(
        "REVOKE {} ON {}.{} FROM {};",
        privilege_list(privs),
        oracle_ident(schema),
        oracle_ident(table),
        oracle_ident(grantee)
    )
}

pub fn alter_user_lock_sql(name: &str, locked: bool) -> String {
    format!(
        "ALTER USER {} ACCOUNT {};",
        oracle_ident(name),
        if locked { "LOCK" } else { "UNLOCK" }
    )
}

pub fn alter_user_password_sql(name: &str, password: &str) -> String {
    format!(
        "ALTER USER {} IDENTIFIED BY {};",
        oracle_ident(name),
        password_literal(password)
    )
}

/// `DROP USER … CASCADE` also drops every object the user owns.
pub fn drop_user_sql(name: &str, cascade: bool) -> String {
    format!(
        "DROP USER {}{};",
        oracle_ident(name),
        if cascade { " CASCADE" } else { "" }
    )
}

pub fn drop_role_sql(name: &str) -> String {
    format!("DROP ROLE {};", oracle_ident(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_sets_keep_the_postgres_column_shapes() {
        let roles = roles_query();
        assert!(roles.contains("dba_users"));
        assert!(roles.contains("dba_roles"));
        for column in [
            "rolsuper",
            "rolcreatedb",
            "rolcreaterole",
            "rolinherit",
            "rolcanlogin",
            "rolconnlimit",
            "rolvaliduntil",
        ] {
            assert!(roles.contains(column), "{column}");
            assert!(roles_query_compat().contains(column), "compat {column}");
        }
        assert!(roles_query_compat().contains("all_users"));
        assert!(!roles_query_compat().contains("dba_"));
        assert!(role_members_query().contains("dba_role_privs"));
        assert!(role_members_query_compat().contains("user_role_privs"));
        assert!(database_connect_grants_query().contains("CREATE SESSION"));
        assert!(database_connect_grants_query().contains("'PUBLIC'"));
        assert!(schema_usage_query().contains("1 = 0"));
        assert!(tables_query().contains("all_tables"));
        assert!(table_grants_query("app").contains("dba_tab_privs"));
        assert!(table_grants_query_compat("app").contains("all_tab_privs"));
        assert!(users_query().contains("account_status"));
        assert!(users_query_compat().contains("account_status"));
        assert!(profiles_query().contains("dba_profiles"));
        assert!(system_privileges_query("APP").contains("dba_sys_privs"));
    }

    #[test]
    fn literals_and_grantees_are_escaped() {
        assert!(table_grants_query("o'brien").contains("'o''brien'"));
        assert_eq!(oracle_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(oracle_new_name("reader"), "\"READER\"");
        assert_eq!(oracle_new_name("app_user$1"), "\"APP_USER$1\"");
        assert_eq!(oracle_new_name("Mixed Case"), "\"Mixed Case\"");
        assert_eq!(oracle_new_name("1abc"), "\"1abc\"");
    }

    #[test]
    fn user_and_role_scripts_render_oracle_ddl() {
        assert_eq!(
            create_user_sql("reader", "s3cr\"et", Some("USERS"), Some("DEFAULT")),
            "CREATE USER \"READER\" IDENTIFIED BY \"s3cret\" DEFAULT TABLESPACE \"USERS\" PROFILE \"DEFAULT\";"
        );
        assert_eq!(
            create_user_sql("reader", "pw", None, Some("  ")),
            "CREATE USER \"READER\" IDENTIFIED BY \"pw\";"
        );
        assert_eq!(create_role_sql("readers"), "CREATE ROLE \"READERS\";");
        assert_eq!(
            grant_role_sql("READERS", "READER", false),
            "GRANT \"READERS\" TO \"READER\";"
        );
        assert_eq!(
            grant_role_sql("DBA", "READER", true),
            "GRANT \"DBA\" TO \"READER\" WITH ADMIN OPTION;"
        );
        assert_eq!(
            revoke_role_sql("READERS", "READER"),
            "REVOKE \"READERS\" FROM \"READER\";"
        );
        assert_eq!(
            grant_create_session_sql("READER"),
            "GRANT CREATE SESSION TO \"READER\";"
        );
        assert_eq!(
            revoke_create_session_sql("READER"),
            "REVOKE CREATE SESSION FROM \"READER\";"
        );
        assert_eq!(
            grant_system_privilege_sql("create role; drop user", "X"),
            "GRANT CREATE ROLE DROP USER TO \"X\";"
        );
        assert_eq!(
            grant_table_sql(
                "HR",
                "EMP",
                &[TablePrivilege::Update, TablePrivilege::Select],
                "READER"
            ),
            "GRANT SELECT, UPDATE ON \"HR\".\"EMP\" TO \"READER\";"
        );
        assert_eq!(
            revoke_table_sql("HR", "EMP", &[TablePrivilege::Delete], "READER"),
            "REVOKE DELETE ON \"HR\".\"EMP\" FROM \"READER\";"
        );
        assert_eq!(
            alter_user_lock_sql("READER", true),
            "ALTER USER \"READER\" ACCOUNT LOCK;"
        );
        assert_eq!(
            alter_user_lock_sql("READER", false),
            "ALTER USER \"READER\" ACCOUNT UNLOCK;"
        );
        assert_eq!(
            alter_user_password_sql("READER", "new"),
            "ALTER USER \"READER\" IDENTIFIED BY \"new\";"
        );
        assert_eq!(
            drop_user_sql("READER", true),
            "DROP USER \"READER\" CASCADE;"
        );
        assert_eq!(drop_user_sql("READER", false), "DROP USER \"READER\";");
        assert_eq!(drop_role_sql("READERS"), "DROP ROLE \"READERS\";");
    }
}
