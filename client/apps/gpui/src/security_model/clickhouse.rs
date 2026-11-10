//! ClickHouse security model: the read-only view the Security dialog shows
//! over `system.users` / `system.roles` / `system.grants` /
//! `system.role_grants`, and the review script it offers for a principal.
//! ClickHouse access control is richer than the dialog's edit forms (host
//! restrictions, settings profiles, quotas, row policies), so the dialog
//! lists, describes and scripts; it never diffs and applies edits.

use sqlly_core::CellValue;
use sqlly_services::admin::security_sql::clickhouse::{GrantRow, principal_script};

use super::{cell_bool, cell_text};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChUserEntry {
    pub name: String,
    /// `sha256_password`, `no_password`, `ldap`, … (several on servers that
    /// allow more than one authentication method).
    pub auth_types: String,
    /// The host restriction in the words of the catalog: IPs, names, LIKE
    /// and regexp patterns, or "any host".
    pub hosts: String,
    /// Default roles: every granted role, or the listed ones.
    pub default_roles: String,
    pub default_database: String,
    /// Where the user is defined: `local_directory` (SQL-driven) or
    /// `users_xml` (the config file, which SQL cannot change).
    pub storage: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChRoleEntry {
    pub name: String,
    pub storage: String,
}

/// One `system.grants` row: a privilege held by a user or a role.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChGrant {
    pub user_name: String,
    pub role_name: String,
    pub row: GrantRow,
}

/// One `system.role_grants` row: a role held by a user or another role.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChRoleGrant {
    pub user_name: String,
    pub role_name: String,
    pub granted_role: String,
    pub is_default: bool,
    pub admin_option: bool,
}

/// `toString(auth_type)` is `sha256_password` on older servers and
/// `['sha256_password','ldap']` on servers with multi-method
/// authentication; both read as a plain comma-separated list.
pub fn normalize_auth_types(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|part| part.trim().trim_matches('\'').to_string())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn host_summary(ip: &str, names: &str, regexp: &str, like: &str) -> String {
    let mut parts = Vec::new();
    if !ip.is_empty() {
        parts.push(format!("IP {ip}"));
    }
    if !names.is_empty() {
        parts.push(format!("NAME {names}"));
    }
    if !regexp.is_empty() {
        parts.push(format!("REGEXP {regexp}"));
    }
    if !like.is_empty() {
        parts.push(format!("LIKE {like}"));
    }
    if parts.is_empty() {
        crate::localization::text("explorer.security.clickhouse_any_host")
    } else {
        parts.join("; ")
    }
}

/// Rows of the users listing: name, auth_type, host_ip, host_names,
/// host_names_regexp, host_names_like, default_roles_all,
/// default_roles_list, default_database, storage.
pub fn parse_users(rows: &[Vec<CellValue>]) -> Vec<ChUserEntry> {
    rows.iter()
        .filter_map(|row| {
            let name = cell_text(row.first()?).trim().to_string();
            if name.is_empty() {
                return None;
            }
            let text = |i: usize| row.get(i).map(cell_text).unwrap_or_default();
            let default_roles = if row.get(6).map(cell_bool).unwrap_or(false) {
                crate::localization::text("explorer.security.clickhouse_all_granted_roles")
            } else {
                let list = text(7);
                if list.is_empty() {
                    crate::localization::text("explorer.security.clickhouse_no_default_roles")
                } else {
                    list
                }
            };
            Some(ChUserEntry {
                name,
                auth_types: normalize_auth_types(&text(1)),
                hosts: host_summary(&text(2), &text(3), &text(4), &text(5)),
                default_roles,
                default_database: text(8),
                storage: text(9),
            })
        })
        .collect()
}

pub fn parse_roles(rows: &[Vec<CellValue>]) -> Vec<ChRoleEntry> {
    rows.iter()
        .filter_map(|row| {
            let name = cell_text(row.first()?).trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some(ChRoleEntry {
                name,
                storage: row.get(1).map(cell_text).unwrap_or_default(),
            })
        })
        .collect()
}

/// Rows of the grants listing: user_name, role_name, access_type,
/// database, table, column, is_partial_revoke, grant_option.
pub fn parse_grants(rows: &[Vec<CellValue>]) -> Vec<ChGrant> {
    rows.iter()
        .filter_map(|row| {
            let text = |i: usize| row.get(i).map(cell_text).unwrap_or_default();
            let access_type = text(2);
            if access_type.is_empty() {
                return None;
            }
            Some(ChGrant {
                user_name: text(0),
                role_name: text(1),
                row: GrantRow {
                    access_type,
                    database: text(3),
                    table: text(4),
                    column: text(5),
                    is_partial_revoke: row.get(6).map(cell_bool).unwrap_or(false),
                    grant_option: row.get(7).map(cell_bool).unwrap_or(false),
                },
            })
        })
        .collect()
}

/// Rows of the role-grants listing: user_name, role_name,
/// granted_role_name, granted_role_is_default, with_admin_option.
pub fn parse_role_grants(rows: &[Vec<CellValue>]) -> Vec<ChRoleGrant> {
    rows.iter()
        .filter_map(|row| {
            let text = |i: usize| row.get(i).map(cell_text).unwrap_or_default();
            let granted_role = text(2);
            if granted_role.is_empty() {
                return None;
            }
            Some(ChRoleGrant {
                user_name: text(0),
                role_name: text(1),
                granted_role,
                is_default: row.get(3).map(cell_bool).unwrap_or(false),
                admin_option: row.get(4).map(cell_bool).unwrap_or(false),
            })
        })
        .collect()
}

/// The privilege rows one principal holds directly.
pub fn grants_for(grants: &[ChGrant], name: &str, is_role: bool) -> Vec<GrantRow> {
    grants
        .iter()
        .filter(|g| {
            if is_role {
                g.role_name == name
            } else {
                g.user_name == name
            }
        })
        .map(|g| g.row.clone())
        .collect()
}

/// The roles one principal holds, as `(role, with admin option)`.
pub fn roles_for(role_grants: &[ChRoleGrant], name: &str, is_role: bool) -> Vec<(String, bool)> {
    role_grants
        .iter()
        .filter(|g| {
            if is_role {
                g.role_name == name
            } else {
                g.user_name == name
            }
        })
        .map(|g| (g.granted_role.clone(), g.admin_option))
        .collect()
}

/// The review script for one principal built from the catalog rows: used
/// when the server's own `SHOW CREATE` / `SHOW GRANTS` text is not
/// readable by the connected user.
pub fn script_from_catalog(
    name: &str,
    is_role: bool,
    grants: &[ChGrant],
    role_grants: &[ChRoleGrant],
) -> String {
    principal_script(
        name,
        is_role,
        &grants_for(grants, name, is_role),
        &roles_for(role_grants, name, is_role),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> CellValue {
        CellValue::String(v.into())
    }

    #[test]
    fn auth_types_read_scalar_and_array_forms() {
        assert_eq!(normalize_auth_types("sha256_password"), "sha256_password");
        assert_eq!(
            normalize_auth_types("['sha256_password','ldap']"),
            "sha256_password, ldap"
        );
        assert_eq!(normalize_auth_types("[]"), "");
    }

    #[test]
    fn users_parse_hosts_and_default_roles() {
        let rows = vec![
            vec![
                s("app"),
                s("['sha256_password']"),
                s("10.0.0.0/8"),
                s(""),
                s(""),
                s("%.example.com"),
                CellValue::I64(0),
                s("reader, writer"),
                s("shop"),
                s("local_directory"),
            ],
            vec![
                s("default"),
                s("no_password"),
                s(""),
                s(""),
                s(""),
                s(""),
                CellValue::I64(1),
                s(""),
                s(""),
                s("users_xml"),
            ],
            vec![s(""), s("x")],
        ];
        let users = parse_users(&rows);
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].name, "app");
        assert_eq!(users[0].auth_types, "sha256_password");
        assert_eq!(users[0].hosts, "IP 10.0.0.0/8; LIKE %.example.com");
        assert_eq!(users[0].default_roles, "reader, writer");
        assert_eq!(users[0].default_database, "shop");
        assert_eq!(users[0].storage, "local_directory");
        assert_eq!(users[1].hosts, "any host");
        assert_eq!(users[1].default_roles, "all granted roles");
        let roles = parse_roles(&[vec![s("reader"), s("local_directory")], vec![s("")]]);
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].name, "reader");
    }

    #[test]
    fn grants_and_role_grants_resolve_per_principal() {
        let grants = parse_grants(&[
            vec![
                s("app"),
                s(""),
                s("SELECT"),
                s("shop"),
                s(""),
                s(""),
                CellValue::I64(0),
                CellValue::I64(0),
            ],
            vec![
                s(""),
                s("reader"),
                s("SELECT"),
                s("shop"),
                s("orders"),
                s("amount"),
                CellValue::I64(0),
                CellValue::I64(1),
            ],
            vec![s("app"), s(""), s(""), s(""), s(""), s("")],
        ]);
        assert_eq!(grants.len(), 2);
        let role_grants = parse_role_grants(&[
            vec![
                s("app"),
                s(""),
                s("reader"),
                CellValue::I64(1),
                CellValue::I64(0),
            ],
            vec![
                s(""),
                s("reader"),
                s("viewer"),
                CellValue::I64(0),
                CellValue::I64(1),
            ],
        ]);
        assert_eq!(grants_for(&grants, "app", false).len(), 1);
        assert_eq!(grants_for(&grants, "reader", true).len(), 1);
        assert!(grants_for(&grants, "reader", false).is_empty());
        assert_eq!(
            roles_for(&role_grants, "app", false),
            vec![("reader".to_string(), false)]
        );
        assert_eq!(
            roles_for(&role_grants, "reader", true),
            vec![("viewer".to_string(), true)]
        );
        let script = script_from_catalog("app", false, &grants, &role_grants);
        assert!(
            script.contains("CREATE USER IF NOT EXISTS `app`"),
            "{script}"
        );
        assert!(
            script.contains("GRANT SELECT ON `shop`.* TO `app`;"),
            "{script}"
        );
        assert!(script.contains("GRANT `reader` TO `app`;"), "{script}");
        let role_script = script_from_catalog("reader", true, &grants, &role_grants);
        assert!(role_script.starts_with("CREATE ROLE IF NOT EXISTS `reader`;"));
        assert!(
            role_script.contains(
                "GRANT SELECT(`amount`) ON `shop`.`orders` TO `reader` WITH GRANT OPTION;"
            ),
            "{role_script}"
        );
        assert!(role_script.contains("GRANT `viewer` TO `reader` WITH ADMIN OPTION;"));
    }
}
