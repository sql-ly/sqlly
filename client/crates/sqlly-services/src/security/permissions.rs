#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionAction {
    Grant,
    Deny,
    Revoke,
}

pub struct SecurityCatalog;

impl SecurityCatalog {
    pub fn server_logins_query() -> &'static str {
        "SELECT name, type_desc, is_disabled, create_date, modify_date FROM sys.server_principals WHERE type IN ('S','U','G','E','X') ORDER BY name"
    }

    pub fn database_users_query() -> &'static str {
        "SELECT name, type_desc, authentication_type_desc, default_schema_name, create_date, modify_date FROM sys.database_principals WHERE type IN ('S','U','G','E','X') ORDER BY name"
    }

    pub fn roles_query() -> &'static str {
        "SELECT name, type_desc, is_fixed_role FROM sys.database_principals WHERE type = 'R' ORDER BY name"
    }

    pub fn role_membership_query() -> &'static str {
        "SELECT role_principal.name AS role_name, member_principal.name AS member_name FROM sys.database_role_members drm JOIN sys.database_principals role_principal ON drm.role_principal_id = role_principal.principal_id JOIN sys.database_principals member_principal ON drm.member_principal_id = member_principal.principal_id ORDER BY role_name, member_name"
    }

    pub fn permissions_query(object_name: Option<&str>) -> String {
        match object_name {
            Some(object) => format!(
                "SELECT pr.name AS principal_name, pe.permission_name, pe.state_desc FROM sys.database_permissions pe JOIN sys.database_principals pr ON pe.grantee_principal_id = pr.principal_id WHERE pe.major_id = OBJECT_ID(N'{}') ORDER BY pr.name, pe.permission_name",
                literal(object)
            ),
            None => "SELECT pr.name AS principal_name, pe.permission_name, pe.state_desc, pe.class_desc FROM sys.database_permissions pe JOIN sys.database_principals pr ON pe.grantee_principal_id = pr.principal_id ORDER BY pr.name, pe.permission_name".to_string(),
        }
    }

    /// Online, non-system databases — the candidates a login can be mapped
    /// into. `database_id > 4` drops master/tempdb/model/msdb; `state = 0` is
    /// ONLINE (a mapping query against an offline database would fail).
    pub fn user_databases_query() -> &'static str {
        "SELECT name FROM sys.databases WHERE database_id > 4 AND state = 0 ORDER BY name"
    }

    /// All server roles (fixed and user-defined) a login can join.
    pub fn server_roles_query() -> &'static str {
        "SELECT name FROM sys.server_principals WHERE type = 'R' ORDER BY name"
    }

    /// Server roles the named login is currently a member of.
    pub fn server_role_memberships_query(login: &str) -> String {
        format!(
            "SELECT role_principal.name AS role_name FROM sys.server_role_members srm JOIN sys.server_principals role_principal ON srm.role_principal_id = role_principal.principal_id JOIN sys.server_principals member_principal ON srm.member_principal_id = member_principal.principal_id WHERE member_principal.name = N'{}' ORDER BY role_name",
            literal(login)
        )
    }

    /// One row per database the login is mapped into, as (database, user).
    /// Built as a UNION ALL over three-part-named `sys.database_principals`
    /// so the whole mapping arrives in a single batch without touching
    /// undocumented helpers like sp_MSforeachdb. Each branch's user name is
    /// pinned to the current database's collation: names carry their own
    /// database's collation, and a UNION across databases with different
    /// collations otherwise fails with Msg 451. Empty database list yields a
    /// query that returns no rows.
    pub fn login_db_user_mapping_query(login: &str, databases: &[String]) -> String {
        let mut sql = format!(
            "DECLARE @sid varbinary(85) = (SELECT sid FROM sys.server_principals WHERE name = N'{}');\n",
            literal(login)
        );
        let branches: Vec<String> = databases
            .iter()
            .map(|db| {
                // Each database's catalog carries its own collation; UNION ALL
                // across databases with different ones fails with Msg 451
                // ("Cannot resolve collation conflict"), so pin every branch to
                // the current database's collation.
                format!(
                    "SELECT N'{}' COLLATE DATABASE_DEFAULT AS database_name, p.name COLLATE DATABASE_DEFAULT AS user_name FROM [{}].sys.database_principals p WHERE p.sid = @sid",
                    literal(db),
                    quote_name(db)
                )
            })
            .collect();
        if branches.is_empty() {
            sql.push_str("SELECT CAST(NULL AS nvarchar(128)) AS database_name, CAST(NULL AS nvarchar(128)) AS user_name WHERE 1 = 0");
        } else {
            sql.push_str(&branches.join("\nUNION ALL\n"));
        }
        sql
    }

    /// Database users with their mapped server login (NULL for users WITHOUT
    /// LOGIN). Built-in principals are excluded — dbo/guest/sys are not
    /// editable principals in this tool.
    pub fn database_users_with_login_query() -> &'static str {
        "SELECT p.name, p.type_desc, p.authentication_type_desc, p.default_schema_name, sp.name AS login_name FROM sys.database_principals p LEFT JOIN sys.server_principals sp ON p.sid = sp.sid WHERE p.type IN ('S','U','G','E','X') AND p.name NOT IN ('dbo','guest','INFORMATION_SCHEMA','sys') ORDER BY p.name"
    }

    /// Prefix a database-scoped statement with an explicit context switch so
    /// the caller never depends on whichever database the session landed in.
    pub fn use_database_sql(database: &str, inner_sql: &str) -> String {
        format!("USE [{}];\n{}", quote_name(database), inner_sql)
    }
}

pub struct SecurityWriter;

impl SecurityWriter {
    pub fn create_login_sql(name: &str, password: &str) -> String {
        format!(
            "CREATE LOGIN [{}] WITH PASSWORD = N'{}'",
            quote_name(name),
            literal(password)
        )
    }

    /// Windows/Entra-authenticated login — no password, the name is a
    /// `DOMAIN\user` or `user@tenant` principal validated by the OS/IdP.
    pub fn create_windows_login_sql(name: &str) -> String {
        format!("CREATE LOGIN [{}] FROM WINDOWS", quote_name(name))
    }

    pub fn alter_login_password_sql(name: &str, new_password: &str) -> String {
        format!(
            "ALTER LOGIN [{}] WITH PASSWORD = N'{}'",
            quote_name(name),
            literal(new_password)
        )
    }

    pub fn alter_login_enabled_sql(name: &str, enabled: bool) -> String {
        let action = if enabled { "ENABLE" } else { "DISABLE" };
        format!("ALTER LOGIN [{}] {action}", quote_name(name))
    }

    /// Server-scope counterpart to [`Self::alter_role_sql`].
    pub fn alter_server_role_sql(role: &str, member: &str, add: bool) -> String {
        let action = if add { "ADD" } else { "DROP" };
        format!(
            "ALTER SERVER ROLE [{}] {action} MEMBER [{}]",
            quote_name(role),
            quote_name(member)
        )
    }

    pub fn alter_user_default_schema_sql(user: &str, schema: &str) -> String {
        format!(
            "ALTER USER [{}] WITH DEFAULT_SCHEMA = [{}]",
            quote_name(user),
            quote_name(schema)
        )
    }

    /// Contained database user: authenticates against the database itself
    /// with its own password, no server login involved. Needs a partially
    /// contained database (or Azure SQL Database).
    pub fn create_contained_user_sql(
        user: &str,
        password: &str,
        default_schema: Option<&str>,
    ) -> String {
        let mut sql = format!(
            "CREATE USER [{}] WITH PASSWORD = N'{}'",
            quote_name(user),
            literal(password)
        );
        if let Some(schema) = default_schema {
            sql.push_str(&format!(", DEFAULT_SCHEMA = [{}]", quote_name(schema)));
        }
        sql
    }

    /// Password reset for a contained database user.
    pub fn alter_user_password_sql(user: &str, new_password: &str) -> String {
        format!(
            "ALTER USER [{}] WITH PASSWORD = N'{}'",
            quote_name(user),
            literal(new_password)
        )
    }

    pub fn drop_login_sql(name: &str) -> String {
        format!("DROP LOGIN [{}]", quote_name(name))
    }

    pub fn drop_user_sql(user: &str) -> String {
        format!("DROP USER [{}]", quote_name(user))
    }

    pub fn create_user_sql(
        user: &str,
        login: Option<&str>,
        default_schema: Option<&str>,
    ) -> String {
        let mut sql = match login {
            Some(login) => format!(
                "CREATE USER [{}] FOR LOGIN [{}]",
                quote_name(user),
                quote_name(login)
            ),
            None => format!("CREATE USER [{}] WITHOUT LOGIN", quote_name(user)),
        };
        if let Some(schema) = default_schema {
            sql.push_str(&format!(" WITH DEFAULT_SCHEMA = [{}]", quote_name(schema)));
        }
        sql
    }

    pub fn alter_role_sql(role: &str, member: &str, add: bool) -> String {
        let action = if add { "ADD" } else { "DROP" };
        format!(
            "ALTER ROLE [{}] {action} MEMBER [{}]",
            quote_name(role),
            quote_name(member)
        )
    }

    pub fn permission_sql(
        action: PermissionAction,
        permission: &str,
        object: &str,
        principal: &str,
    ) -> String {
        match action {
            PermissionAction::Grant => format!(
                "GRANT {permission} ON OBJECT::{} TO [{}]",
                multipart(object),
                quote_name(principal)
            ),
            PermissionAction::Deny => format!(
                "DENY {permission} ON OBJECT::{} TO [{}]",
                multipart(object),
                quote_name(principal)
            ),
            PermissionAction::Revoke => format!(
                "REVOKE {permission} ON OBJECT::{} FROM [{}]",
                multipart(object),
                quote_name(principal)
            ),
        }
    }
}

fn quote_name(name: &str) -> String {
    name.replace(']', "]]")
}

fn literal(value: &str) -> String {
    value.replace('\'', "''")
}

fn multipart(name: &str) -> String {
    name.split('.')
        .map(|part| format!("[{}]", quote_name(part)))
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_security_catalog_queries() {
        assert!(SecurityCatalog::server_logins_query().contains("server_principals"));
        assert!(SecurityCatalog::database_users_query().contains("database_principals"));
        assert!(SecurityCatalog::role_membership_query().contains("database_role_members"));
        assert!(SecurityCatalog::permissions_query(Some("dbo.Orders")).contains("OBJECT_ID"));
    }

    #[test]
    fn scripts_security_writes() {
        assert!(SecurityWriter::create_login_sql("app]login", "p'ass").contains("p''ass"));
        assert!(
            SecurityWriter::create_user_sql("app", Some("login"), Some("dbo"))
                .contains("DEFAULT_SCHEMA")
        );
        assert!(
            SecurityWriter::alter_role_sql("db_datareader", "app", true).contains("ADD MEMBER")
        );
        assert!(
            SecurityWriter::permission_sql(PermissionAction::Grant, "SELECT", "dbo.Orders", "app")
                .contains("GRANT SELECT")
        );
    }

    #[test]
    fn scripts_contained_user_and_drop_writes() {
        assert_eq!(
            SecurityWriter::create_contained_user_sql("app]x", "p'w", Some("sales")),
            "CREATE USER [app]]x] WITH PASSWORD = N'p''w', DEFAULT_SCHEMA = [sales]"
        );
        assert_eq!(
            SecurityWriter::create_contained_user_sql("app", "pw", None),
            "CREATE USER [app] WITH PASSWORD = N'pw'"
        );
        assert_eq!(
            SecurityWriter::alter_user_password_sql("app", "n'ew"),
            "ALTER USER [app] WITH PASSWORD = N'n''ew'"
        );
        assert_eq!(SecurityWriter::drop_login_sql("a]b"), "DROP LOGIN [a]]b]");
    }

    #[test]
    fn scripts_login_lifecycle_writes() {
        assert_eq!(
            SecurityWriter::create_windows_login_sql("CONTOSO\\analyst"),
            "CREATE LOGIN [CONTOSO\\analyst] FROM WINDOWS"
        );
        assert_eq!(
            SecurityWriter::alter_login_password_sql("app", "n'ew"),
            "ALTER LOGIN [app] WITH PASSWORD = N'n''ew'"
        );
        assert_eq!(
            SecurityWriter::alter_login_enabled_sql("app", false),
            "ALTER LOGIN [app] DISABLE"
        );
        assert_eq!(
            SecurityWriter::alter_login_enabled_sql("app", true),
            "ALTER LOGIN [app] ENABLE"
        );
        assert_eq!(
            SecurityWriter::alter_server_role_sql("sysadmin", "app", true),
            "ALTER SERVER ROLE [sysadmin] ADD MEMBER [app]"
        );
        assert_eq!(
            SecurityWriter::alter_server_role_sql("sysadmin", "app", false),
            "ALTER SERVER ROLE [sysadmin] DROP MEMBER [app]"
        );
        assert_eq!(
            SecurityWriter::alter_user_default_schema_sql("app", "sales"),
            "ALTER USER [app] WITH DEFAULT_SCHEMA = [sales]"
        );
        assert_eq!(SecurityWriter::drop_user_sql("app"), "DROP USER [app]");
    }

    #[test]
    fn builds_security_dialog_catalog_queries() {
        assert!(SecurityCatalog::user_databases_query().contains("sys.databases"));
        assert!(SecurityCatalog::server_roles_query().contains("type = 'R'"));
        assert!(SecurityCatalog::server_role_memberships_query("o'hare").contains("N'o''hare'"));
        assert!(
            SecurityCatalog::database_users_with_login_query()
                .contains("LEFT JOIN sys.server_principals")
        );
        let mapping =
            SecurityCatalog::login_db_user_mapping_query("app", &["A".into(), "B".into()]);
        // Mixed-collation databases must not break the UNION ALL (Msg 451).
        assert_eq!(
            mapping.matches("p.name COLLATE DATABASE_DEFAULT").count(),
            2
        );
        let scoped = SecurityCatalog::use_database_sql("my]db", "SELECT 1");
        assert!(scoped.starts_with("USE [my]]db]"));
    }

    #[test]
    fn mapping_query_unions_per_database_and_quotes() {
        let sql = SecurityCatalog::login_db_user_mapping_query(
            "analyst",
            &["Reporting".to_string(), "Sales]East".to_string()],
        );
        assert!(sql.contains("DECLARE @sid"));
        assert!(sql.contains("N'analyst'"));
        assert!(sql.contains("FROM [Reporting].sys.database_principals"));
        assert!(sql.contains("FROM [Sales]]East].sys.database_principals"));
        assert!(sql.contains("UNION ALL"));
    }

    /// Regression (Msg 451): user names from databases with different
    /// collations cannot be UNIONed unless each branch is pinned to one.
    #[test]
    fn mapping_query_pins_every_branch_to_one_collation() {
        let sql = SecurityCatalog::login_db_user_mapping_query(
            "analyst",
            &["Reporting".to_string(), "Archive".to_string()],
        );
        assert_eq!(
            sql.matches("p.name COLLATE DATABASE_DEFAULT AS user_name")
                .count(),
            2,
            "{sql}"
        );
    }

    #[test]
    fn mapping_query_with_no_databases_returns_an_empty_set() {
        let sql = SecurityCatalog::login_db_user_mapping_query("analyst", &[]);
        assert!(sql.contains("WHERE 1 = 0"));
        assert!(!sql.contains("UNION ALL"));
    }
}
