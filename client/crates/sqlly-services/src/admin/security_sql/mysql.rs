use super::{my_ident, my_literal, mysql_account};

pub const GLOBAL_PRIVILEGES: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "ALTER",
    "INDEX",
    "CREATE VIEW",
    "SHOW VIEW",
    "EXECUTE",
    "PROCESS",
    "RELOAD",
    "SUPER",
];

pub const DATABASE_PRIVILEGES: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "ALTER",
    "INDEX",
    "CREATE VIEW",
    "SHOW VIEW",
    "EXECUTE",
];

pub const TABLE_PRIVILEGES: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE"];

pub fn users_query() -> &'static str {
    "SELECT User, Host, account_locked, plugin FROM mysql.user ORDER BY User, Host"
}

pub fn users_query_compat() -> &'static str {
    "SELECT User, Host, 'N' AS account_locked, plugin FROM mysql.user ORDER BY User, Host"
}

pub fn show_grants_sql(user: &str, host: &str) -> String {
    format!("SHOW GRANTS FOR {}", mysql_account(user, Some(host)))
}

pub fn roles_query() -> &'static str {
    "SELECT User, Host FROM mysql.user WHERE account_locked = 'Y' AND authentication_string = '' ORDER BY User"
}

pub fn databases_query() -> &'static str {
    "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME NOT IN ('mysql','sys','information_schema','performance_schema') ORDER BY 1"
}

pub fn use_database_sql(database: &str, inner_sql: &str) -> String {
    format!("USE {};\n{}", my_ident(database), inner_sql)
}

pub fn create_user_sql(user: &str, host: &str, password: Option<&str>) -> String {
    let mut sql = format!("CREATE USER {}", mysql_account(user, Some(host)));
    if let Some(password) = password {
        sql.push_str(&format!(" IDENTIFIED BY {}", my_literal(password)));
    }
    sql
}

pub fn alter_user_password_sql(user: &str, host: &str, password: &str) -> String {
    format!(
        "ALTER USER {} IDENTIFIED BY {}",
        mysql_account(user, Some(host)),
        my_literal(password)
    )
}

pub fn alter_user_lock_sql(user: &str, host: &str, locked: bool) -> String {
    format!(
        "ALTER USER {} ACCOUNT {}",
        mysql_account(user, Some(host)),
        if locked { "LOCK" } else { "UNLOCK" }
    )
}

pub fn drop_user_sql(user: &str, host: &str) -> String {
    format!("DROP USER {}", mysql_account(user, Some(host)))
}

fn render_privileges(privs: &[&str]) -> String {
    privs.join(", ")
}

pub fn grant_global_sql(privs: &[&str], user: &str, host: &str) -> String {
    format!(
        "GRANT {} ON *.* TO {}",
        render_privileges(privs),
        mysql_account(user, Some(host))
    )
}

pub fn revoke_global_sql(privs: &[&str], user: &str, host: &str) -> String {
    format!(
        "REVOKE {} ON *.* FROM {}",
        render_privileges(privs),
        mysql_account(user, Some(host))
    )
}

pub fn grant_database_sql(db: &str, privs: &[&str], user: &str, host: &str) -> String {
    format!(
        "GRANT {} ON {}.* TO {}",
        render_privileges(privs),
        my_ident(db),
        mysql_account(user, Some(host))
    )
}

pub fn revoke_database_sql(db: &str, privs: &[&str], user: &str, host: &str) -> String {
    format!(
        "REVOKE {} ON {}.* FROM {}",
        render_privileges(privs),
        my_ident(db),
        mysql_account(user, Some(host))
    )
}

pub fn grant_table_sql(db: &str, table: &str, privs: &[&str], user: &str, host: &str) -> String {
    format!(
        "GRANT {} ON {}.{} TO {}",
        render_privileges(privs),
        my_ident(db),
        my_ident(table),
        mysql_account(user, Some(host))
    )
}

pub fn revoke_table_sql(db: &str, table: &str, privs: &[&str], user: &str, host: &str) -> String {
    format!(
        "REVOKE {} ON {}.{} FROM {}",
        render_privileges(privs),
        my_ident(db),
        my_ident(table),
        mysql_account(user, Some(host))
    )
}

pub fn create_role_sql(role: &str) -> String {
    format!("CREATE ROLE {}", my_literal(role))
}

pub fn drop_role_sql(role: &str) -> String {
    format!("DROP ROLE {}", my_literal(role))
}

pub fn grant_role_sql(role: &str, user: &str, host: &str) -> String {
    format!(
        "GRANT {} TO {}",
        mysql_account(role, Some("%")),
        mysql_account(user, Some(host))
    )
}

pub fn revoke_role_sql(role: &str, user: &str, host: &str) -> String {
    format!(
        "REVOKE {} FROM {}",
        mysql_account(role, Some("%")),
        mysql_account(user, Some(host))
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantScope {
    Global,
    Database(String),
    Table(String, String),
    Role(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantLine {
    pub privileges: Vec<String>,
    pub scope: GrantScope,
}

fn privileges_for_scope(scope: &GrantScope, raw: &[String]) -> Vec<String> {
    if raw.iter().any(|p| p.eq_ignore_ascii_case("ALL PRIVILEGES")) {
        let list = match scope {
            GrantScope::Global => GLOBAL_PRIVILEGES,
            GrantScope::Database(_) => DATABASE_PRIVILEGES,
            GrantScope::Table(_, _) => TABLE_PRIVILEGES,
            GrantScope::Role(_) => return Vec::new(),
        };
        return list.iter().map(|p| (*p).to_string()).collect();
    }
    raw.to_vec()
}

fn split_privileges(segment: &str) -> Vec<String> {
    segment
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn parse_account(segment: &str) -> Option<(String, String)> {
    let segment = segment.trim();
    if !segment.starts_with('\'') {
        return None;
    }
    let rest = &segment[1..];
    let (user, rest) = rest.split_once('\'')?;
    let rest = rest.trim_start();
    if !rest.starts_with('@') {
        return None;
    }
    let rest = rest[1..].trim();
    if !rest.starts_with('\'') {
        return None;
    }
    let (host, _) = rest[1..].split_once('\'')?;
    Some((user.replace("''", "'"), host.replace("''", "'")))
}

fn parse_on_target(on: &str) -> Option<GrantScope> {
    let on = on.trim();
    if on == "*.*" {
        return Some(GrantScope::Global);
    }
    if let Some(db) = on.strip_suffix(".*") {
        let db = db.trim().trim_matches('`');
        return Some(GrantScope::Database(db.to_string()));
    }
    if let Some((db, table)) = on.split_once('.') {
        let db = db.trim().trim_matches('`');
        let table = table.trim().trim_matches('`');
        return Some(GrantScope::Table(db.to_string(), table.to_string()));
    }
    None
}

/// Parse `SHOW GRANTS` output lines. Unknown lines are skipped.
pub fn parse_show_grants(lines: &[String]) -> Vec<GrantLine> {
    let mut out = Vec::new();
    for line in lines {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("GRANT ")
            .or_else(|| line.strip_prefix("grant "))
        else {
            continue;
        };
        let Some((privs_part, tail)) = rest.split_once(" ON ") else {
            // Role grant: GRANT `role`@`%` TO `user`@`host`
            if let Some((role_part, to_part)) = rest.split_once(" TO ")
                && let Some((role, _)) = parse_account(role_part.trim())
                && (to_part.trim().starts_with('\'') || to_part.trim().starts_with('`'))
            {
                out.push(GrantLine {
                    privileges: Vec::new(),
                    scope: GrantScope::Role(role),
                });
            }
            continue;
        };
        let Some((on_part, to_part)) = tail.split_once(" TO ") else {
            continue;
        };
        let privs = split_privileges(privs_part);
        if privs.iter().any(|p| p.eq_ignore_ascii_case("USAGE")) && on_part.trim() == "*.*" {
            continue;
        }
        if let Some(scope) = parse_on_target(on_part) {
            if matches!(
                scope,
                GrantScope::Global | GrantScope::Database(_) | GrantScope::Table(_, _)
            ) && to_part.trim().starts_with('\'')
            {
                let expanded = privileges_for_scope(&scope, &privs);
                if !expanded.is_empty() {
                    out.push(GrantLine {
                        privileges: expanded,
                        scope,
                    });
                }
            }
        } else if let Some((role, _)) = parse_account(on_part.trim()) {
            out.push(GrantLine {
                privileges: Vec::new(),
                scope: GrantScope::Role(role),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_create_user_and_grants_render_exactly() {
        assert_eq!(
            create_user_sql("app", "%", Some("p@ss")),
            "CREATE USER 'app'@'%' IDENTIFIED BY 'p@ss'"
        );
        assert_eq!(
            grant_database_sql("sales", &["SELECT", "INSERT"], "app", "%"),
            "GRANT SELECT, INSERT ON `sales`.* TO 'app'@'%'"
        );
    }

    #[test]
    fn mysql_parse_show_grants_reads_global_db_table_and_role() {
        let lines = vec![
            "GRANT SELECT, INSERT ON *.* TO 'app'@'%'".to_string(),
            "GRANT UPDATE ON `sales`.* TO 'app'@'%'".to_string(),
            "GRANT DELETE ON `sales`.`orders` TO 'app'@'%'".to_string(),
            "GRANT USAGE ON *.* TO 'app'@'%'".to_string(),
            "GRANT 'reader'@'%' TO 'app'@'%'".to_string(),
        ];
        let grants = parse_show_grants(&lines);
        assert!(grants.iter().any(|g| matches!(g.scope, GrantScope::Global)));
        assert!(
            grants
                .iter()
                .any(|g| matches!(&g.scope, GrantScope::Database(db) if db == "sales"))
        );
        assert!(grants.iter().any(
            |g| matches!(&g.scope, GrantScope::Table(db, table) if db == "sales" && table == "orders")
        ));
        assert!(
            grants
                .iter()
                .any(|g| matches!(&g.scope, GrantScope::Role(role) if role == "reader"))
        );
    }

    #[test]
    fn mysql_parse_show_grants_expands_all_privileges() {
        let lines = vec!["GRANT ALL PRIVILEGES ON `app`.* TO 'u'@'localhost'".to_string()];
        let grants = parse_show_grants(&lines);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].privileges.len(), DATABASE_PRIVILEGES.len());
    }
}
