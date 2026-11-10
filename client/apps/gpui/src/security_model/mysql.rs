use std::collections::{BTreeMap, BTreeSet};

use sqlly_core::CellValue;
use sqlly_services::admin::security_sql::mysql::{self, GrantLine, GrantScope};

pub use sqlly_services::admin::security_sql::mysql::{
    DATABASE_PRIVILEGES, GLOBAL_PRIVILEGES, parse_show_grants,
};

use super::{PlannedStatement, cell_bool, cell_text};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MyUserEntry {
    pub user: String,
    pub host: String,
    pub locked: bool,
    pub is_role: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct MyUserBaseline {
    pub user: String,
    pub host: String,
    pub locked: bool,
    pub global: BTreeSet<String>,
    pub databases: BTreeMap<String, BTreeSet<String>>,
    pub tables: BTreeMap<(String, String), BTreeSet<String>>,
    pub roles: BTreeSet<String>,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct MyUserForm {
    pub user: String,
    pub host: String,
    pub password: String,
    pub confirm: String,
    pub locked: bool,
    pub global: BTreeSet<String>,
    pub databases: BTreeMap<String, BTreeSet<String>>,
    pub tables: BTreeMap<(String, String), BTreeSet<String>>,
    pub roles: BTreeSet<String>,
}

pub fn parse_users(rows: &[Vec<CellValue>], role_names: &BTreeSet<String>) -> Vec<MyUserEntry> {
    rows.iter()
        .filter_map(|row| {
            let user = cell_text(row.first()?).trim().to_string();
            let host = cell_text(row.get(1)?).trim().to_string();
            if user.is_empty() {
                return None;
            }
            let locked = row.get(2).map(cell_bool).unwrap_or(false);
            let is_role = role_names.contains(&user);
            Some(MyUserEntry {
                user,
                host,
                locked,
                is_role,
            })
        })
        .collect()
}

pub fn baseline_from_grants(
    user: &str,
    host: &str,
    locked: bool,
    grants: &[GrantLine],
) -> MyUserBaseline {
    let mut baseline = MyUserBaseline {
        user: user.to_string(),
        host: host.to_string(),
        locked,
        ..MyUserBaseline::default()
    };
    for grant in grants {
        match &grant.scope {
            GrantScope::Global => baseline.global.extend(grant.privileges.clone()),
            GrantScope::Database(db) => {
                baseline
                    .databases
                    .entry(db.clone())
                    .or_default()
                    .extend(grant.privileges.clone());
            }
            GrantScope::Table(db, table) => {
                baseline
                    .tables
                    .entry((db.clone(), table.clone()))
                    .or_default()
                    .extend(grant.privileges.clone());
            }
            GrantScope::Role(role) => {
                baseline.roles.insert(role.clone());
            }
        }
    }
    baseline
}

fn account_key(user: &str, host: &str) -> (String, String) {
    let host = if host.trim().is_empty() {
        "%".to_string()
    } else {
        host.trim().to_string()
    };
    (user.trim().to_string(), host)
}

pub fn validate(
    is_new: bool,
    existing: &BTreeSet<(String, String)>,
    form: &MyUserForm,
) -> Vec<String> {
    let mut errors = Vec::new();
    let user = form.user.trim();
    if user.is_empty() {
        errors.push(crate::localization::text(
            "explorer.security.mysql_user_name_required",
        ));
    }
    let (user_key, host_key) = account_key(user, &form.host);
    if is_new && existing.contains(&(user_key.clone(), host_key.clone())) {
        errors.push(super::statement_description(
            "explorer.security.mysql_account_exists",
            &[("user", &user_key), ("host", &host_key)],
        ));
    }
    if is_new && form.password.is_empty() {
        errors.push(crate::localization::text(
            "explorer.security.mysql_new_password_required",
        ));
    }
    if !form.password.is_empty() && form.password != form.confirm {
        errors.push(crate::localization::text(
            "explorer.security.mysql_passwords_do_not_match",
        ));
    }
    errors
}

fn privilege_vec(set: &BTreeSet<String>) -> Vec<&str> {
    set.iter().map(String::as_str).collect()
}

pub fn diff(
    baseline: Option<&MyUserBaseline>,
    form: &MyUserForm,
    roles_supported: bool,
) -> Vec<PlannedStatement> {
    let (user, host) = account_key(&form.user, &form.host);
    if user.is_empty() {
        return Vec::new();
    }
    let account = format!("{user}@{host}");
    let description =
        |key, arguments: &[(&str, &str)]| super::statement_description(key, arguments);
    let mut statements = Vec::new();
    match baseline {
        None => {
            let password = if form.password.is_empty() {
                None
            } else {
                Some(form.password.as_str())
            };
            statements.push(PlannedStatement::server(
                mysql::create_user_sql(&user, &host, password),
                description(
                    "explorer.security.mysql_create_user",
                    &[("account", &account)],
                ),
            ));
            if form.locked {
                statements.push(PlannedStatement::server(
                    mysql::alter_user_lock_sql(&user, &host, true),
                    description(
                        "explorer.security.mysql_lock_account",
                        &[("account", &account)],
                    ),
                ));
            }
            if !form.global.is_empty() {
                statements.push(PlannedStatement::server(
                    mysql::grant_global_sql(&privilege_vec(&form.global), &user, &host),
                    description(
                        "explorer.security.mysql_grant_global_privileges",
                        &[("account", &account)],
                    ),
                ));
            }
            for (db, privs) in &form.databases {
                if privs.is_empty() {
                    continue;
                }
                statements.push(PlannedStatement::server(
                    mysql::grant_database_sql(db, &privilege_vec(privs), &user, &host),
                    description(
                        "explorer.security.mysql_grant_database_privileges",
                        &[("database", db), ("account", &account)],
                    ),
                ));
            }
            for ((db, table), privs) in &form.tables {
                if privs.is_empty() {
                    continue;
                }
                statements.push(PlannedStatement::server(
                    mysql::grant_table_sql(db, table, &privilege_vec(privs), &user, &host),
                    description(
                        "explorer.security.mysql_grant_table_privileges",
                        &[("database", db), ("table", table), ("account", &account)],
                    ),
                ));
            }
            if roles_supported {
                for role in &form.roles {
                    statements.push(PlannedStatement::server(
                        mysql::grant_role_sql(role, &user, &host),
                        description(
                            "explorer.security.mysql_grant_role",
                            &[("role", role), ("account", &account)],
                        ),
                    ));
                }
            }
        }
        Some(base) => {
            if !form.password.is_empty() {
                statements.push(PlannedStatement::server(
                    mysql::alter_user_password_sql(&user, &host, &form.password),
                    description(
                        "explorer.security.mysql_reset_password",
                        &[("account", &account)],
                    ),
                ));
            }
            if form.locked != base.locked {
                let key = if form.locked {
                    "explorer.security.mysql_lock_account"
                } else {
                    "explorer.security.mysql_unlock_account"
                };
                statements.push(PlannedStatement::server(
                    mysql::alter_user_lock_sql(&user, &host, form.locked),
                    description(key, &[("account", &account)]),
                ));
            }
            for priv_name in form.global.difference(&base.global) {
                statements.push(PlannedStatement::server(
                    mysql::grant_global_sql(&[priv_name.as_str()], &user, &host),
                    description(
                        "explorer.security.mysql_grant_global_privilege",
                        &[("privilege", priv_name), ("account", &account)],
                    ),
                ));
            }
            for priv_name in base.global.difference(&form.global) {
                statements.push(PlannedStatement::server(
                    mysql::revoke_global_sql(&[priv_name.as_str()], &user, &host),
                    description(
                        "explorer.security.mysql_revoke_global_privilege",
                        &[("privilege", priv_name), ("account", &account)],
                    ),
                ));
            }
            let db_keys: BTreeSet<_> = base
                .databases
                .keys()
                .chain(form.databases.keys())
                .cloned()
                .collect();
            for db in db_keys {
                let base_privs = base.databases.get(&db).cloned().unwrap_or_default();
                let form_privs = form.databases.get(&db).cloned().unwrap_or_default();
                let added: Vec<_> = form_privs.difference(&base_privs).cloned().collect();
                let removed: Vec<_> = base_privs.difference(&form_privs).cloned().collect();
                if !added.is_empty() {
                    let refs: Vec<&str> = added.iter().map(String::as_str).collect();
                    statements.push(PlannedStatement::server(
                        mysql::grant_database_sql(&db, &refs, &user, &host),
                        description(
                            "explorer.security.mysql_grant_database_privileges",
                            &[("database", &db), ("account", &account)],
                        ),
                    ));
                }
                if !removed.is_empty() {
                    let refs: Vec<&str> = removed.iter().map(String::as_str).collect();
                    statements.push(PlannedStatement::server(
                        mysql::revoke_database_sql(&db, &refs, &user, &host),
                        description(
                            "explorer.security.mysql_revoke_database_privileges",
                            &[("database", &db), ("account", &account)],
                        ),
                    ));
                }
            }
            let table_keys: BTreeSet<_> = base
                .tables
                .keys()
                .chain(form.tables.keys())
                .cloned()
                .collect();
            for key in table_keys {
                let base_privs = base.tables.get(&key).cloned().unwrap_or_default();
                let form_privs = form.tables.get(&key).cloned().unwrap_or_default();
                let added: Vec<_> = form_privs.difference(&base_privs).cloned().collect();
                let removed: Vec<_> = base_privs.difference(&form_privs).cloned().collect();
                let (db, table) = &key;
                if !added.is_empty() {
                    let refs: Vec<&str> = added.iter().map(String::as_str).collect();
                    statements.push(PlannedStatement::server(
                        mysql::grant_table_sql(db, table, &refs, &user, &host),
                        description(
                            "explorer.security.mysql_grant_table_privileges",
                            &[("database", db), ("table", table), ("account", &account)],
                        ),
                    ));
                }
                if !removed.is_empty() {
                    let refs: Vec<&str> = removed.iter().map(String::as_str).collect();
                    statements.push(PlannedStatement::server(
                        mysql::revoke_table_sql(db, table, &refs, &user, &host),
                        description(
                            "explorer.security.mysql_revoke_table_privileges",
                            &[("database", db), ("table", table), ("account", &account)],
                        ),
                    ));
                }
            }
            if roles_supported {
                for role in form.roles.difference(&base.roles) {
                    statements.push(PlannedStatement::server(
                        mysql::grant_role_sql(role, &user, &host),
                        description(
                            "explorer.security.mysql_grant_role",
                            &[("role", role), ("account", &account)],
                        ),
                    ));
                }
                for role in base.roles.difference(&form.roles) {
                    statements.push(PlannedStatement::server(
                        mysql::revoke_role_sql(role, &user, &host),
                        description(
                            "explorer.security.mysql_revoke_role",
                            &[("role", role), ("account", &account)],
                        ),
                    ));
                }
            }
        }
    }
    statements
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn my_new_user_scripts_create_then_grants() {
        let mut form = MyUserForm {
            user: "app".to_string(),
            host: "%".to_string(),
            password: "secret".to_string(),
            confirm: "secret".to_string(),
            global: ["SELECT".to_string()].into_iter().collect(),
            ..MyUserForm::default()
        };
        form.databases.insert(
            "sales".to_string(),
            ["INSERT".to_string()].into_iter().collect(),
        );
        let statements = diff(None, &form, true);
        assert!(statements.len() >= 3);
        assert!(statements[0].sql.contains("CREATE USER"));
        assert!(statements.iter().any(|s| s.sql.contains("ON *.*")));
        assert!(statements.iter().any(|s| s.sql.contains("`sales`.*")));
    }

    #[test]
    fn my_baseline_from_show_grants_round_trips_into_empty_diff() {
        let grants = mysql::parse_show_grants(&[
            "GRANT SELECT ON *.* TO 'app'@'%'".to_string(),
            "GRANT INSERT ON `sales`.* TO 'app'@'%'".to_string(),
        ]);
        let baseline = baseline_from_grants("app", "%", false, &grants);
        let form = MyUserForm {
            user: "app".to_string(),
            host: "%".to_string(),
            global: baseline.global.clone(),
            databases: baseline.databases.clone(),
            ..MyUserForm::default()
        };
        assert!(diff(Some(&baseline), &form, false).is_empty());
    }

    #[test]
    fn my_validate_defaults_host_to_percent() {
        let existing = BTreeSet::new();
        let form = MyUserForm {
            user: "app".to_string(),
            password: "secret".to_string(),
            confirm: "secret".to_string(),
            ..MyUserForm::default()
        };
        assert!(validate(true, &existing, &form).is_empty());
        let key = account_key("app", "");
        assert_eq!(key, ("app".to_string(), "%".to_string()));
    }

    #[test]
    fn my_roles_skipped_when_unsupported() {
        let form = MyUserForm {
            user: "app".to_string(),
            host: "%".to_string(),
            password: "secret".to_string(),
            confirm: "secret".to_string(),
            roles: ["reader".to_string()].into_iter().collect(),
            ..MyUserForm::default()
        };
        let statements = diff(None, &form, false);
        assert!(!statements.iter().any(|s| s.sql.contains("GRANT 'reader'")));
        let with_roles = diff(None, &form, true);
        assert!(with_roles.iter().any(|s| s.sql.contains("GRANT 'reader'")));
    }
}
