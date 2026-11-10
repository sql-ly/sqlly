use std::collections::{BTreeMap, BTreeSet};

use sqlly_core::CellValue;
use sqlly_services::admin::security_sql::postgres;

pub use sqlly_services::admin::security_sql::postgres::{RoleOptions, TablePrivilege};

use super::{PlannedStatement, cell_bool, cell_text, name_taken};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PgRoleEntry {
    pub name: String,
    pub login: bool,
    pub superuser: bool,
    pub createdb: bool,
    pub createrole: bool,
    pub inherit: bool,
}

impl PgRoleEntry {
    /// The role's flags as the server reports them: the baseline the
    /// editor's checkboxes diff against.
    pub fn opts(&self) -> RoleOptions {
        RoleOptions {
            login: self.login,
            superuser: self.superuser,
            createdb: self.createdb,
            createrole: self.createrole,
            inherit: self.inherit,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PgRoleBaseline {
    pub name: String,
    pub opts: RoleOptions,
    pub member_of: BTreeSet<String>,
    pub connect_dbs: BTreeSet<String>,
    pub schema_usage: BTreeSet<String>,
    pub table_grants: BTreeMap<(String, String), BTreeSet<TablePrivilege>>,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PgRoleForm {
    pub name: String,
    pub password: String,
    pub confirm: String,
    pub opts: RoleOptions,
    pub member_of: BTreeSet<String>,
    pub connect_dbs: BTreeSet<String>,
    pub schema_usage: BTreeSet<String>,
    pub table_grants: BTreeMap<(String, String), BTreeSet<TablePrivilege>>,
}

pub fn parse_roles(rows: &[Vec<CellValue>]) -> Vec<PgRoleEntry> {
    rows.iter()
        .filter_map(|row| {
            let name = cell_text(row.first()?).trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some(PgRoleEntry {
                name,
                superuser: row.get(1).map(cell_bool).unwrap_or(false),
                createdb: row.get(2).map(cell_bool).unwrap_or(false),
                createrole: row.get(3).map(cell_bool).unwrap_or(false),
                inherit: row.get(4).map(cell_bool).unwrap_or(true),
                login: row.get(5).map(cell_bool).unwrap_or(false),
            })
        })
        .collect()
}

pub fn parse_role_members(rows: &[Vec<CellValue>]) -> Vec<(String, String, bool)> {
    rows.iter()
        .filter_map(|row| {
            let role = cell_text(row.first()?).trim().to_string();
            let member = cell_text(row.get(1)?).trim().to_string();
            if role.is_empty() || member.is_empty() {
                return None;
            }
            Some((role, member, row.get(2).map(cell_bool).unwrap_or(false)))
        })
        .collect()
}

pub fn parse_pairs(rows: &[Vec<CellValue>]) -> Vec<(String, String)> {
    rows.iter()
        .filter_map(|row| {
            let left = cell_text(row.first()?).trim().to_string();
            let right = cell_text(row.get(1)?).trim().to_string();
            if left.is_empty() || right.is_empty() {
                return None;
            }
            Some((left, right))
        })
        .collect()
}

fn parse_table_privilege(value: &str) -> Option<TablePrivilege> {
    match value.to_ascii_uppercase().as_str() {
        "SELECT" => Some(TablePrivilege::Select),
        "INSERT" => Some(TablePrivilege::Insert),
        "UPDATE" => Some(TablePrivilege::Update),
        "DELETE" => Some(TablePrivilege::Delete),
        _ => None,
    }
}

pub fn parse_table_grants(
    rows: &[Vec<CellValue>],
) -> BTreeMap<(String, String), BTreeSet<TablePrivilege>> {
    let mut out: BTreeMap<(String, String), BTreeSet<TablePrivilege>> = BTreeMap::new();
    for row in rows {
        let schema = cell_text(row.first().unwrap_or(&CellValue::Null))
            .trim()
            .to_string();
        let table = cell_text(row.get(1).unwrap_or(&CellValue::Null))
            .trim()
            .to_string();
        let priv_type = cell_text(row.get(2).unwrap_or(&CellValue::Null));
        if schema.is_empty() || table.is_empty() {
            continue;
        }
        if let Some(privilege) = parse_table_privilege(&priv_type) {
            out.entry((schema, table)).or_default().insert(privilege);
        }
    }
    out
}

pub fn validate(is_new: bool, existing: &BTreeSet<String>, form: &PgRoleForm) -> Vec<String> {
    let mut errors = Vec::new();
    let name = form.name.trim();
    if name.is_empty() {
        errors.push(crate::localization::text(
            "explorer.security.postgres_role_name_required",
        ));
    } else if is_new && name_taken(existing, name) {
        errors.push(super::statement_description(
            "explorer.security.postgres_role_exists",
            &[("name", name)],
        ));
    }
    if form.opts.login && is_new && form.password.is_empty() {
        errors.push(crate::localization::text(
            "explorer.security.postgres_new_password_required",
        ));
    }
    if !form.password.is_empty() && form.password != form.confirm {
        errors.push(crate::localization::text(
            "explorer.security.postgres_passwords_do_not_match",
        ));
    }
    errors
}

/// Which engine's DDL the Roles tab scripts. Oracle rides the same form:
/// a login role is an Oracle user, a non-login role an Oracle role,
/// "connect to database" is `CREATE SESSION`, and schema usage does not
/// exist (the form never offers it there).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RoleScriptDialect {
    #[default]
    Postgres,
    Oracle,
}

pub fn diff_for(
    dialect: RoleScriptDialect,
    baseline: Option<&PgRoleBaseline>,
    form: &PgRoleForm,
) -> Vec<PlannedStatement> {
    match dialect {
        RoleScriptDialect::Postgres => diff_postgres(baseline, form),
        RoleScriptDialect::Oracle => diff_oracle(baseline, form),
    }
}

/// The Oracle plan for the same form: `CREATE USER` / `CREATE ROLE`, role
/// grants, `CREATE SESSION` for the connect flag, and table grants. An
/// existing account cannot change between user and role, and the DBA /
/// CREATE ROLE flags map onto the DBA role and the CREATE ROLE privilege.
fn diff_oracle(baseline: Option<&PgRoleBaseline>, form: &PgRoleForm) -> Vec<PlannedStatement> {
    use sqlly_services::admin::security_sql::oracle;
    let name = form.name.trim();
    if name.is_empty() {
        return Vec::new();
    }
    let mut statements = Vec::new();
    match baseline {
        None => {
            let created = if form.opts.login {
                oracle::create_user_sql(name, &form.password, None, None)
            } else {
                oracle::create_role_sql(name)
            };
            statements.push(PlannedStatement::server(
                created,
                if form.opts.login {
                    super::statement_description(
                        "explorer.security.plan_oracle_create_user",
                        &[("name", name)],
                    )
                } else {
                    super::statement_description(
                        "explorer.security.plan_create_role",
                        &[("name", name)],
                    )
                },
            ));
            // Scripts after CREATE address the account by its stored name.
            let stored = oracle_stored_name(name);
            let name = stored.as_str();
            if form.opts.superuser {
                statements.push(PlannedStatement::server(
                    oracle::grant_role_sql("DBA", name, false),
                    super::statement_description(
                        "explorer.security.plan_oracle_grant_dba",
                        &[("name", name)],
                    ),
                ));
            }
            if form.opts.createrole {
                statements.push(PlannedStatement::server(
                    oracle::grant_system_privilege_sql("CREATE ROLE", name),
                    super::statement_description(
                        "explorer.security.plan_oracle_grant_create_role",
                        &[("name", name)],
                    ),
                ));
            }
            for role in &form.member_of {
                statements.push(PlannedStatement::server(
                    oracle::grant_role_sql(role, name, false),
                    super::statement_description(
                        "explorer.security.plan_grant_role",
                        &[("role", role), ("name", name)],
                    ),
                ));
            }
            if !form.connect_dbs.is_empty() {
                statements.push(PlannedStatement::server(
                    oracle::grant_create_session_sql(name),
                    super::statement_description(
                        "explorer.security.plan_oracle_grant_create_session",
                        &[("name", name)],
                    ),
                ));
            }
            for ((schema, table), privs) in &form.table_grants {
                if privs.is_empty() {
                    continue;
                }
                let privs_vec: Vec<_> = privs.iter().copied().collect();
                statements.push(PlannedStatement::server(
                    oracle::grant_table_sql(schema, table, &privs_vec, name),
                    super::statement_description(
                        "explorer.security.plan_grant_table_privileges",
                        &[("schema", schema), ("table", table), ("name", name)],
                    ),
                ));
            }
        }
        Some(base) => {
            if base.opts.login != form.opts.login {
                statements.push(PlannedStatement::server(
                    format!(
                        "-- Oracle cannot turn the user {name} into a role (or back); drop and recreate it instead"
                    ),
                    super::statement_description(
                        "explorer.security.plan_oracle_cannot_change_login",
                        &[("name", name)],
                    ),
                ));
            }
            if base.opts.superuser != form.opts.superuser {
                statements.push(PlannedStatement::server(
                    if form.opts.superuser {
                        oracle::grant_role_sql("DBA", name, false)
                    } else {
                        oracle::revoke_role_sql("DBA", name)
                    },
                    super::statement_description(
                        "explorer.security.plan_oracle_change_dba",
                        &[("name", name)],
                    ),
                ));
            }
            if base.opts.createrole != form.opts.createrole {
                statements.push(PlannedStatement::server(
                    if form.opts.createrole {
                        oracle::grant_system_privilege_sql("CREATE ROLE", name)
                    } else {
                        oracle::revoke_system_privilege_sql("CREATE ROLE", name)
                    },
                    super::statement_description(
                        "explorer.security.plan_oracle_change_create_role",
                        &[("name", name)],
                    ),
                ));
            }
            if !form.password.is_empty() {
                statements.push(PlannedStatement::server(
                    oracle::alter_user_password_sql(name, &form.password),
                    super::statement_description(
                        "explorer.security.plan_reset_password",
                        &[("name", name)],
                    ),
                ));
            }
            for role in form.member_of.difference(&base.member_of) {
                statements.push(PlannedStatement::server(
                    oracle::grant_role_sql(role, name, false),
                    super::statement_description(
                        "explorer.security.plan_grant_role",
                        &[("role", role), ("name", name)],
                    ),
                ));
            }
            for role in base.member_of.difference(&form.member_of) {
                statements.push(PlannedStatement::server(
                    oracle::revoke_role_sql(role, name),
                    super::statement_description(
                        "explorer.security.plan_revoke_role",
                        &[("role", role), ("name", name)],
                    ),
                ));
            }
            if base.connect_dbs.is_empty() && !form.connect_dbs.is_empty() {
                statements.push(PlannedStatement::server(
                    oracle::grant_create_session_sql(name),
                    super::statement_description(
                        "explorer.security.plan_oracle_grant_create_session",
                        &[("name", name)],
                    ),
                ));
            }
            if !base.connect_dbs.is_empty() && form.connect_dbs.is_empty() {
                statements.push(PlannedStatement::server(
                    oracle::revoke_create_session_sql(name),
                    super::statement_description(
                        "explorer.security.plan_oracle_revoke_create_session",
                        &[("name", name)],
                    ),
                ));
            }
            let table_keys: BTreeSet<_> = base
                .table_grants
                .keys()
                .chain(form.table_grants.keys())
                .cloned()
                .collect();
            for key in table_keys {
                let base_privs = base.table_grants.get(&key).cloned().unwrap_or_default();
                let form_privs = form.table_grants.get(&key).cloned().unwrap_or_default();
                let added: Vec<_> = form_privs.difference(&base_privs).copied().collect();
                let removed: Vec<_> = base_privs.difference(&form_privs).copied().collect();
                let (schema, table) = &key;
                if !added.is_empty() {
                    statements.push(PlannedStatement::server(
                        oracle::grant_table_sql(schema, table, &added, name),
                        super::statement_description(
                            "explorer.security.plan_grant_table_privileges",
                            &[("schema", schema), ("table", table), ("name", name)],
                        ),
                    ));
                }
                if !removed.is_empty() {
                    statements.push(PlannedStatement::server(
                        oracle::revoke_table_sql(schema, table, &removed, name),
                        super::statement_description(
                            "explorer.security.plan_revoke_table_privileges",
                            &[("schema", schema), ("table", table), ("name", name)],
                        ),
                    ));
                }
            }
        }
    }
    statements
}

/// The name Oracle stores for a typed account name: the quoted form's
/// contents (upper-cased when it was a plain identifier).
fn oracle_stored_name(name: &str) -> String {
    let quoted = sqlly_services::admin::security_sql::oracle::oracle_new_name(name);
    quoted.trim_matches('"').replace("\"\"", "\"").to_string()
}

fn diff_postgres(baseline: Option<&PgRoleBaseline>, form: &PgRoleForm) -> Vec<PlannedStatement> {
    let name = form.name.trim();
    if name.is_empty() {
        return Vec::new();
    }
    let mut statements = Vec::new();
    match baseline {
        None => {
            let password = if form.opts.login && !form.password.is_empty() {
                Some(form.password.as_str())
            } else {
                None
            };
            statements.push(PlannedStatement::server(
                postgres::create_role_sql(name, &form.opts, password),
                super::statement_description(
                    "explorer.security.plan_create_role",
                    &[("name", name)],
                ),
            ));
            for role in &form.member_of {
                statements.push(PlannedStatement::server(
                    postgres::grant_role_sql(role, name, false),
                    super::statement_description(
                        "explorer.security.plan_grant_role",
                        &[("role", role), ("name", name)],
                    ),
                ));
            }
            for db in &form.connect_dbs {
                statements.push(PlannedStatement::server(
                    postgres::grant_connect_sql(db, name),
                    super::statement_description(
                        "explorer.security.plan_grant_connect",
                        &[("database", db), ("name", name)],
                    ),
                ));
            }
            for schema in &form.schema_usage {
                statements.push(PlannedStatement::server(
                    postgres::grant_schema_usage_sql(schema, name),
                    super::statement_description(
                        "explorer.security.plan_grant_schema_usage",
                        &[("schema", schema), ("name", name)],
                    ),
                ));
            }
            for ((schema, table), privs) in &form.table_grants {
                if privs.is_empty() {
                    continue;
                }
                let privs_vec: Vec<_> = privs.iter().copied().collect();
                statements.push(PlannedStatement::server(
                    postgres::grant_table_sql(schema, table, &privs_vec, name),
                    super::statement_description(
                        "explorer.security.plan_grant_table_privileges",
                        &[("schema", schema), ("table", table), ("name", name)],
                    ),
                ));
            }
        }
        Some(base) => {
            if base.opts != form.opts {
                statements.push(PlannedStatement::server(
                    postgres::alter_role_options_sql(name, &form.opts),
                    super::statement_description(
                        "explorer.security.plan_alter_role_options",
                        &[("name", name)],
                    ),
                ));
            }
            if !form.password.is_empty() {
                statements.push(PlannedStatement::server(
                    postgres::alter_role_password_sql(name, &form.password),
                    super::statement_description(
                        "explorer.security.plan_reset_password",
                        &[("name", name)],
                    ),
                ));
            }
            for role in form.member_of.difference(&base.member_of) {
                statements.push(PlannedStatement::server(
                    postgres::grant_role_sql(role, name, false),
                    super::statement_description(
                        "explorer.security.plan_grant_role",
                        &[("role", role), ("name", name)],
                    ),
                ));
            }
            for role in base.member_of.difference(&form.member_of) {
                statements.push(PlannedStatement::server(
                    postgres::revoke_role_sql(role, name),
                    super::statement_description(
                        "explorer.security.plan_revoke_role",
                        &[("role", role), ("name", name)],
                    ),
                ));
            }
            for db in form.connect_dbs.difference(&base.connect_dbs) {
                statements.push(PlannedStatement::server(
                    postgres::grant_connect_sql(db, name),
                    super::statement_description(
                        "explorer.security.plan_grant_connect",
                        &[("database", db), ("name", name)],
                    ),
                ));
            }
            for db in base.connect_dbs.difference(&form.connect_dbs) {
                statements.push(PlannedStatement::server(
                    postgres::revoke_connect_sql(db, name),
                    super::statement_description(
                        "explorer.security.plan_revoke_connect",
                        &[("database", db), ("name", name)],
                    ),
                ));
            }
            for schema in form.schema_usage.difference(&base.schema_usage) {
                statements.push(PlannedStatement::server(
                    postgres::grant_schema_usage_sql(schema, name),
                    super::statement_description(
                        "explorer.security.plan_grant_schema_usage",
                        &[("schema", schema), ("name", name)],
                    ),
                ));
            }
            for schema in base.schema_usage.difference(&form.schema_usage) {
                statements.push(PlannedStatement::server(
                    postgres::revoke_schema_usage_sql(schema, name),
                    super::statement_description(
                        "explorer.security.plan_revoke_schema_usage",
                        &[("schema", schema), ("name", name)],
                    ),
                ));
            }
            let table_keys: BTreeSet<_> = base
                .table_grants
                .keys()
                .chain(form.table_grants.keys())
                .cloned()
                .collect();
            for key in table_keys {
                let base_privs = base.table_grants.get(&key).cloned().unwrap_or_default();
                let form_privs = form.table_grants.get(&key).cloned().unwrap_or_default();
                let added: Vec<_> = form_privs.difference(&base_privs).copied().collect();
                let removed: Vec<_> = base_privs.difference(&form_privs).copied().collect();
                if !added.is_empty() {
                    let (schema, table) = &key;
                    statements.push(PlannedStatement::server(
                        postgres::grant_table_sql(schema, table, &added, name),
                        super::statement_description(
                            "explorer.security.plan_grant_table_privileges",
                            &[("schema", schema), ("table", table), ("name", name)],
                        ),
                    ));
                }
                if !removed.is_empty() {
                    let (schema, table) = &key;
                    statements.push(PlannedStatement::server(
                        postgres::revoke_table_sql(schema, table, &removed, name),
                        super::statement_description(
                            "explorer.security.plan_revoke_table_privileges",
                            &[("schema", schema), ("table", table), ("name", name)],
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
    use sqlly_services::admin::security_sql::postgres::RoleOptions;

    #[test]
    fn pg_new_login_role_scripts_create_then_membership_then_grants() {
        let mut form = PgRoleForm {
            name: "reader".to_string(),
            password: "secret".to_string(),
            confirm: "secret".to_string(),
            opts: RoleOptions {
                login: true,
                ..RoleOptions::default()
            },
            member_of: ["readers".to_string()].into_iter().collect(),
            connect_dbs: ["app".to_string()].into_iter().collect(),
            schema_usage: ["public".to_string()].into_iter().collect(),
            ..PgRoleForm::default()
        };
        let statements = diff_for(RoleScriptDialect::Postgres, None, &form);
        assert!(statements.len() >= 4);
        assert!(statements[0].sql.contains("CREATE ROLE"));
        assert!(
            statements
                .iter()
                .any(|s| s.sql.contains("GRANT \"readers\""))
        );
        assert!(statements.iter().any(|s| s.sql.contains("CONNECT")));
        assert!(statements.iter().any(|s| s.sql.contains("USAGE ON SCHEMA")));
        form.table_grants.insert(
            ("public".to_string(), "orders".to_string()),
            [TablePrivilege::Select].into_iter().collect(),
        );
        let with_table = diff_for(RoleScriptDialect::Postgres, None, &form);
        assert!(with_table.iter().any(|s| s.sql.contains("ON TABLE")));
    }

    #[test]
    fn oracle_new_user_scripts_create_user_session_roles_and_grants() {
        let mut form = PgRoleForm {
            name: "reader".to_string(),
            password: "secret".to_string(),
            confirm: "secret".to_string(),
            opts: RoleOptions {
                login: true,
                ..RoleOptions::default()
            },
            member_of: ["READERS".to_string()].into_iter().collect(),
            connect_dbs: ["FREEPDB1".to_string()].into_iter().collect(),
            schema_usage: ["HR".to_string()].into_iter().collect(),
            ..PgRoleForm::default()
        };
        form.table_grants.insert(
            ("HR".to_string(), "EMP".to_string()),
            [TablePrivilege::Select].into_iter().collect(),
        );
        let statements = diff_for(RoleScriptDialect::Oracle, None, &form);
        let sql: Vec<&str> = statements.iter().map(|s| s.sql.as_str()).collect();
        assert_eq!(
            sql,
            vec![
                "CREATE USER \"READER\" IDENTIFIED BY \"secret\";",
                "GRANT \"READERS\" TO \"READER\";",
                "GRANT CREATE SESSION TO \"READER\";",
                "GRANT SELECT ON \"HR\".\"EMP\" TO \"READER\";",
            ]
        );
        // A non-login role is CREATE ROLE, and schema usage never scripts.
        form.opts.login = false;
        let statements = diff_for(RoleScriptDialect::Oracle, None, &form);
        assert!(statements[0].sql.starts_with("CREATE ROLE \"READER\""));
        assert!(!statements.iter().any(|s| s.sql.contains("USAGE")));
    }

    #[test]
    fn oracle_existing_user_diff_scripts_only_what_changed() {
        let base = PgRoleBaseline {
            name: "READER".to_string(),
            opts: RoleOptions {
                login: true,
                inherit: true,
                ..RoleOptions::default()
            },
            member_of: ["READERS".to_string()].into_iter().collect(),
            connect_dbs: ["FREEPDB1".to_string()].into_iter().collect(),
            table_grants: BTreeMap::from([(
                ("HR".to_string(), "EMP".to_string()),
                [TablePrivilege::Select, TablePrivilege::Insert]
                    .into_iter()
                    .collect(),
            )]),
            ..PgRoleBaseline::default()
        };
        let mut form = PgRoleForm {
            name: "READER".to_string(),
            opts: base.opts,
            member_of: base.member_of.clone(),
            connect_dbs: base.connect_dbs.clone(),
            table_grants: base.table_grants.clone(),
            ..PgRoleForm::default()
        };
        assert!(diff_for(RoleScriptDialect::Oracle, Some(&base), &form).is_empty());
        form.opts.superuser = true;
        form.password = "new".to_string();
        form.member_of.remove("READERS");
        form.member_of.insert("WRITERS".to_string());
        form.connect_dbs.clear();
        form.table_grants
            .get_mut(&("HR".to_string(), "EMP".to_string()))
            .unwrap()
            .remove(&TablePrivilege::Insert);
        let sql: Vec<String> = diff_for(RoleScriptDialect::Oracle, Some(&base), &form)
            .into_iter()
            .map(|s| s.sql)
            .collect();
        assert_eq!(
            sql,
            vec![
                "GRANT \"DBA\" TO \"READER\";",
                "ALTER USER \"READER\" IDENTIFIED BY \"new\";",
                "GRANT \"WRITERS\" TO \"READER\";",
                "REVOKE \"READERS\" FROM \"READER\";",
                "REVOKE CREATE SESSION FROM \"READER\";",
                "REVOKE INSERT ON \"HR\".\"EMP\" FROM \"READER\";",
            ]
        );
        // Turning a user into a role is refused as a comment, never DDL.
        form = PgRoleForm {
            name: "READER".to_string(),
            opts: RoleOptions {
                login: false,
                inherit: true,
                ..RoleOptions::default()
            },
            member_of: base.member_of.clone(),
            connect_dbs: base.connect_dbs.clone(),
            table_grants: base.table_grants.clone(),
            ..PgRoleForm::default()
        };
        let statements = diff_for(RoleScriptDialect::Oracle, Some(&base), &form);
        assert_eq!(statements.len(), 1);
        assert!(statements[0].sql.starts_with("-- Oracle cannot"));
        assert_eq!(oracle_stored_name("reader"), "READER");
        assert_eq!(oracle_stored_name("Mixed Case"), "Mixed Case");
    }

    #[test]
    fn pg_unchanged_form_yields_no_statements() {
        let form = PgRoleForm {
            name: "reader".to_string(),
            ..PgRoleForm::default()
        };
        let base = PgRoleBaseline {
            name: "reader".to_string(),
            ..PgRoleBaseline::default()
        };
        assert!(diff_for(RoleScriptDialect::Postgres, Some(&base), &form).is_empty());
    }

    #[test]
    fn pg_validate_requires_password_for_new_login_role() {
        let existing = BTreeSet::new();
        let form = PgRoleForm {
            name: "reader".to_string(),
            opts: RoleOptions {
                login: true,
                ..RoleOptions::default()
            },
            ..PgRoleForm::default()
        };
        let expected =
            crate::localization::text("explorer.security.postgres_new_password_required");
        assert!(
            validate(true, &existing, &form)
                .iter()
                .any(|error| error == &expected),
            "missing {expected}"
        );
    }

    #[test]
    fn pg_revoking_table_privilege_emits_revoke_for_removed_set_only() {
        let mut base = PgRoleBaseline {
            name: "reader".to_string(),
            table_grants: BTreeMap::from([(
                ("public".to_string(), "orders".to_string()),
                [TablePrivilege::Select, TablePrivilege::Insert]
                    .into_iter()
                    .collect(),
            )]),
            ..PgRoleBaseline::default()
        };
        let mut form = PgRoleForm {
            name: "reader".to_string(),
            table_grants: base.table_grants.clone(),
            ..PgRoleForm::default()
        };
        form.table_grants
            .get_mut(&("public".to_string(), "orders".to_string()))
            .unwrap()
            .remove(&TablePrivilege::Insert);
        let statements = diff_for(RoleScriptDialect::Postgres, Some(&base), &form);
        assert_eq!(statements.len(), 1);
        assert!(statements[0].sql.starts_with("REVOKE INSERT"));
        base.table_grants = form.table_grants.clone();
        assert!(diff_for(RoleScriptDialect::Postgres, Some(&base), &form).is_empty());
    }
}
