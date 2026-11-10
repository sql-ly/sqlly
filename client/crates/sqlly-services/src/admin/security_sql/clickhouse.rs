//! ClickHouse security catalog: read-only listings over `system.users`,
//! `system.roles`, `system.grants`, `system.role_grants` and
//! `system.settings_profiles`, plus `CREATE USER` / `CREATE ROLE` / `GRANT`
//! renderers so the dialog can script a principal for review.
//!
//! ClickHouse's access control is SQL-driven (`CREATE USER … IDENTIFIED WITH
//! … HOST …`, `GRANT … ON db.table TO …`) and richer than the dialog's forms
//! model (host restrictions, settings profiles, row policies, quotas), so the
//! dialog lists and describes principals and hands the user a script; it does
//! not diff and apply edits.

/// Backtick-quote a ClickHouse identifier, escaping embedded `` ` `` and
/// `\` (ClickHouse reads backslash escapes inside backquoted names).
pub(crate) fn ch_ident(s: &str) -> String {
    sqlly_core::DatabaseType::ClickHouse.quote_identifier(s)
}

/// Single-quote a ClickHouse string literal: backslash is an escape
/// character, so it is doubled along with the quote.
pub(crate) fn ch_literal(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

/// Every user with its authentication kinds, host restrictions, default
/// roles and default database. `auth_type` is a scalar enum on older servers
/// and an array of them on newer ones; `toString` renders both and the
/// caller strips the array punctuation.
pub fn users_query() -> &'static str {
    "SELECT name, toString(auth_type) AS auth_type, arrayStringConcat(host_ip, ', ') AS host_ip, arrayStringConcat(host_names, ', ') AS host_names, arrayStringConcat(host_names_regexp, ', ') AS host_names_regexp, arrayStringConcat(host_names_like, ', ') AS host_names_like, default_roles_all, arrayStringConcat(default_roles_list, ', ') AS default_roles_list, default_database, storage FROM system.users ORDER BY name"
}

pub fn roles_query() -> &'static str {
    "SELECT name, storage FROM system.roles ORDER BY name"
}

/// Every privilege grant, for users and roles alike. `access_type` is an
/// enum of the privilege name (`SELECT`, `ALTER UPDATE`, …); NULL
/// database / table / column mean "all".
pub fn grants_query() -> &'static str {
    "SELECT ifNull(user_name, '') AS user_name, ifNull(role_name, '') AS role_name, toString(access_type) AS access_type, ifNull(database, '') AS database, ifNull(table, '') AS table, ifNull(column, '') AS column, is_partial_revoke, grant_option FROM system.grants ORDER BY user_name, role_name, access_type, database, table, column"
}

/// Role membership: which roles a user or role holds, whether each is a
/// default role and whether it was granted WITH ADMIN OPTION.
pub fn role_grants_query() -> &'static str {
    "SELECT ifNull(user_name, '') AS user_name, ifNull(role_name, '') AS role_name, granted_role_name, granted_role_is_default, with_admin_option FROM system.role_grants ORDER BY user_name, role_name, granted_role_name"
}

/// Settings profiles and who they apply to.
pub fn settings_profiles_query() -> &'static str {
    "SELECT name, storage, num_elements, apply_to_all, arrayStringConcat(apply_to_list, ', ') AS apply_to_list FROM system.settings_profiles ORDER BY name"
}

/// The server's own `CREATE USER` text for a user (password hashes are
/// shown as hashes, never in clear).
pub fn show_create_user_sql(name: &str) -> String {
    format!("SHOW CREATE USER {}", ch_ident(name))
}

pub fn show_create_role_sql(name: &str) -> String {
    format!("SHOW CREATE ROLE {}", ch_ident(name))
}

/// The server's own `GRANT` lines for a user or role.
pub fn show_grants_sql(name: &str) -> String {
    format!("SHOW GRANTS FOR {}", ch_ident(name))
}

/// Which host restriction a scripted user gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRestriction {
    Any,
    Ip(String),
    Name(String),
    Like(String),
    Regexp(String),
}

/// `CREATE USER IF NOT EXISTS … IDENTIFIED WITH sha256_password BY '…' HOST …`;
/// no password scripts `NOT IDENTIFIED` (the server's "no password" form),
/// which the reviewer sees plainly before running.
pub fn create_user_sql(
    name: &str,
    password: Option<&str>,
    host: &HostRestriction,
    default_roles: &[String],
) -> String {
    let mut sql = format!("CREATE USER IF NOT EXISTS {}", ch_ident(name));
    match password {
        Some(password) => sql.push_str(&format!(
            " IDENTIFIED WITH sha256_password BY {}",
            ch_literal(password)
        )),
        None => sql.push_str(" NOT IDENTIFIED"),
    }
    match host {
        HostRestriction::Any => sql.push_str(" HOST ANY"),
        HostRestriction::Ip(ip) => sql.push_str(&format!(" HOST IP {}", ch_literal(ip))),
        HostRestriction::Name(name) => sql.push_str(&format!(" HOST NAME {}", ch_literal(name))),
        HostRestriction::Like(pattern) => {
            sql.push_str(&format!(" HOST LIKE {}", ch_literal(pattern)))
        }
        HostRestriction::Regexp(pattern) => {
            sql.push_str(&format!(" HOST REGEXP {}", ch_literal(pattern)))
        }
    }
    if !default_roles.is_empty() {
        let roles: Vec<String> = default_roles.iter().map(|r| ch_ident(r)).collect();
        sql.push_str(&format!(" DEFAULT ROLE {}", roles.join(", ")));
    }
    sql
}

pub fn create_role_sql(name: &str) -> String {
    format!("CREATE ROLE IF NOT EXISTS {}", ch_ident(name))
}

pub fn drop_user_sql(name: &str) -> String {
    format!("DROP USER IF EXISTS {}", ch_ident(name))
}

pub fn drop_role_sql(name: &str) -> String {
    format!("DROP ROLE IF EXISTS {}", ch_ident(name))
}

/// One row of `system.grants`, as the dialog reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRow {
    pub access_type: String,
    pub database: String,
    pub table: String,
    pub column: String,
    pub is_partial_revoke: bool,
    pub grant_option: bool,
}

/// The `ON` target of a grant row: `*.*`, `db.*`, `db.table`, with an
/// optional column list in parentheses after the privilege.
fn grant_target(row: &GrantRow) -> String {
    match (row.database.is_empty(), row.table.is_empty()) {
        (true, _) => "*.*".to_string(),
        (false, true) => format!("{}.*", ch_ident(&row.database)),
        (false, false) => format!("{}.{}", ch_ident(&row.database), ch_ident(&row.table)),
    }
}

/// Render one `system.grants` row back into the `GRANT` / `REVOKE`
/// statement that produced it, addressed to `grantee`.
pub fn grant_statement_sql(grantee: &str, row: &GrantRow) -> String {
    let privilege = if row.column.is_empty() {
        row.access_type.clone()
    } else {
        format!("{}({})", row.access_type, ch_ident(&row.column))
    };
    let target = grant_target(row);
    if row.is_partial_revoke {
        format!("REVOKE {privilege} ON {target} FROM {}", ch_ident(grantee))
    } else {
        let option = if row.grant_option {
            " WITH GRANT OPTION"
        } else {
            ""
        };
        format!(
            "GRANT {privilege} ON {target} TO {}{option}",
            ch_ident(grantee)
        )
    }
}

/// `GRANT role TO grantee [WITH ADMIN OPTION]`.
pub fn grant_role_sql(role: &str, grantee: &str, admin_option: bool) -> String {
    let option = if admin_option {
        " WITH ADMIN OPTION"
    } else {
        ""
    };
    format!("GRANT {} TO {}{option}", ch_ident(role), ch_ident(grantee))
}

/// The review script for one principal: its `CREATE` statement (a
/// placeholder password for users — the real hash is never known to the
/// dialog), then every privilege grant and role grant it holds.
pub fn principal_script(
    name: &str,
    is_role: bool,
    grants: &[GrantRow],
    roles: &[(String, bool)],
) -> String {
    let mut lines = Vec::new();
    if is_role {
        lines.push(create_role_sql(name));
    } else {
        lines.push(format!(
            "-- Replace the password before running; the server never reveals the stored hash.\n{}",
            create_user_sql(name, Some("change-me"), &HostRestriction::Any, &[])
        ));
    }
    for row in grants {
        lines.push(grant_statement_sql(name, row));
    }
    for (role, admin) in roles {
        lines.push(grant_role_sql(role, name, *admin));
    }
    lines
        .into_iter()
        .map(|line| format!("{line};"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ch_ident_escapes_backslashes() {
        assert_eq!(ch_ident(r"u\"), r"`u\\`");
    }

    #[test]
    fn identifiers_and_literals_are_escaped() {
        assert_eq!(ch_ident("we`ird"), "`we``ird`");
        assert_eq!(ch_literal("o'brien\\x"), "'o''brien\\\\x'");
    }

    #[test]
    fn listing_queries_read_the_access_system_tables() {
        assert!(users_query().contains("FROM system.users"));
        assert!(users_query().contains("toString(auth_type)"));
        assert!(roles_query().contains("FROM system.roles"));
        assert!(grants_query().contains("FROM system.grants"));
        assert!(grants_query().contains("is_partial_revoke"));
        assert!(role_grants_query().contains("FROM system.role_grants"));
        assert!(settings_profiles_query().contains("FROM system.settings_profiles"));
        assert_eq!(show_grants_sql("app"), "SHOW GRANTS FOR `app`");
        assert_eq!(show_create_user_sql("a`b"), "SHOW CREATE USER `a``b`");
        assert_eq!(show_create_role_sql("r"), "SHOW CREATE ROLE `r`");
    }

    #[test]
    fn create_user_renders_auth_host_and_default_roles() {
        assert_eq!(
            create_user_sql(
                "app",
                Some("p@ss"),
                &HostRestriction::Ip("10.0.0.0/8".into()),
                &["reader".to_string(), "writer".to_string()]
            ),
            "CREATE USER IF NOT EXISTS `app` IDENTIFIED WITH sha256_password BY 'p@ss' HOST IP '10.0.0.0/8' DEFAULT ROLE `reader`, `writer`"
        );
        assert_eq!(
            create_user_sql("svc", None, &HostRestriction::Any, &[]),
            "CREATE USER IF NOT EXISTS `svc` NOT IDENTIFIED HOST ANY"
        );
        assert!(
            create_user_sql(
                "x",
                None,
                &HostRestriction::Like("%.example.com".into()),
                &[]
            )
            .contains("HOST LIKE '%.example.com'")
        );
        assert!(
            create_user_sql("x", None, &HostRestriction::Regexp("^db.*".into()), &[])
                .contains("HOST REGEXP '^db.*'")
        );
        assert!(
            create_user_sql("x", None, &HostRestriction::Name("host1".into()), &[])
                .contains("HOST NAME 'host1'")
        );
        assert_eq!(
            create_role_sql("reader"),
            "CREATE ROLE IF NOT EXISTS `reader`"
        );
        assert_eq!(drop_user_sql("app"), "DROP USER IF EXISTS `app`");
        assert_eq!(drop_role_sql("reader"), "DROP ROLE IF EXISTS `reader`");
    }

    fn row(access: &str, db: &str, table: &str, column: &str) -> GrantRow {
        GrantRow {
            access_type: access.into(),
            database: db.into(),
            table: table.into(),
            column: column.into(),
            is_partial_revoke: false,
            grant_option: false,
        }
    }

    #[test]
    fn grant_rows_render_each_target_shape() {
        assert_eq!(
            grant_statement_sql("app", &row("SELECT", "", "", "")),
            "GRANT SELECT ON *.* TO `app`"
        );
        assert_eq!(
            grant_statement_sql("app", &row("INSERT", "sales", "", "")),
            "GRANT INSERT ON `sales`.* TO `app`"
        );
        assert_eq!(
            grant_statement_sql("app", &row("SELECT", "sales", "orders", "amount")),
            "GRANT SELECT(`amount`) ON `sales`.`orders` TO `app`"
        );
        let mut option = row("ALTER UPDATE", "sales", "orders", "");
        option.grant_option = true;
        assert_eq!(
            grant_statement_sql("app", &option),
            "GRANT ALTER UPDATE ON `sales`.`orders` TO `app` WITH GRANT OPTION"
        );
        let mut revoke = row("SELECT", "secrets", "", "");
        revoke.is_partial_revoke = true;
        assert_eq!(
            grant_statement_sql("app", &revoke),
            "REVOKE SELECT ON `secrets`.* FROM `app`"
        );
        assert_eq!(
            grant_role_sql("reader", "app", true),
            "GRANT `reader` TO `app` WITH ADMIN OPTION"
        );
    }

    #[test]
    fn principal_script_lists_create_then_grants_then_roles() {
        let script = principal_script(
            "app",
            false,
            &[row("SELECT", "sales", "", "")],
            &[("reader".to_string(), false)],
        );
        let lines: Vec<&str> = script.lines().collect();
        assert_eq!(
            lines[0],
            "-- Replace the password before running; the server never reveals the stored hash."
        );
        assert!(lines[1].starts_with("CREATE USER IF NOT EXISTS `app`"));
        assert_eq!(lines[2], "GRANT SELECT ON `sales`.* TO `app`;");
        assert_eq!(lines[3], "GRANT `reader` TO `app`;");
        let role = principal_script("reader", true, &[], &[]);
        assert_eq!(role, "CREATE ROLE IF NOT EXISTS `reader`;");
    }
}
