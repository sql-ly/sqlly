pub mod clickhouse;
pub mod mysql;
pub mod postgres;

use sqlly_core::{CellValue, DatabaseType};
use sqlly_services::AdminToolsService;

/// One SQL statement the Security dialog proposes to run. `database` scopes it
/// (`USE` prefix on SQL Server / MySQL); `None` is server scope.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlannedStatement {
    pub database: Option<String>,
    pub sql: String,
    pub summary: String,
}

impl PlannedStatement {
    pub fn server(sql: String, summary: String) -> Self {
        Self {
            database: None,
            sql,
            summary: localize_planned_summary(summary),
        }
    }

    pub fn in_db(database: &str, sql: String, summary: String) -> Self {
        Self {
            database: Some(database.to_string()),
            sql,
            summary: localize_planned_summary(summary),
        }
    }

    /// The runnable batch text: bare statement for server scope, `USE`-prefixed
    /// for database scope when the engine requires it.
    pub fn batch_text(&self, admin: &dyn AdminToolsService, db: DatabaseType) -> String {
        match &self.database {
            Some(database) => admin.security_use_database_sql_for(db, database, &self.sql),
            None => self.sql.clone(),
        }
    }
}

fn localize_planned_summary(summary: String) -> String {
    let text = summary.as_str();
    let unary = |prefix: &str, key: &'static str| {
        text.strip_prefix(prefix)
            .map(|name| statement_description(key, &[("name", name)]))
    };
    for (prefix, key) in [
        (
            "create login ",
            "explorer.security.plan_sqlserver_create_login",
        ),
        (
            "disable login ",
            "explorer.security.plan_sqlserver_disable_login",
        ),
        (
            "enable login ",
            "explorer.security.plan_sqlserver_enable_login",
        ),
        (
            "reset the password for ",
            "explorer.security.plan_sqlserver_reset_password",
        ),
        ("drop login ", "explorer.security.plan_sqlserver_drop_login"),
    ] {
        if let Some(localized) = unary(prefix, key) {
            return localized;
        }
    }
    if let Some(rest) = text.strip_prefix("reset the password of login ") {
        return statement_description(
            "explorer.security.plan_sqlserver_reset_login_password",
            &[("login", rest)],
        );
    }
    for (prefix, middle, key, add) in [
        (
            "add ",
            " to server role ",
            "explorer.security.plan_sqlserver_add_server_role",
            true,
        ),
        (
            "remove ",
            " from server role ",
            "explorer.security.plan_sqlserver_remove_server_role",
            false,
        ),
        (
            "add ",
            " to role ",
            "explorer.security.plan_sqlserver_add_role",
            true,
        ),
        (
            "remove ",
            " from role ",
            "explorer.security.plan_sqlserver_remove_role",
            false,
        ),
    ] {
        if let Some(rest) = text.strip_prefix(prefix)
            && let Some((name, role)) = rest.split_once(middle)
        {
            let key = if add {
                match key {
                    "explorer.security.plan_sqlserver_add_server_role" => {
                        "explorer.security.plan_sqlserver_add_server_role"
                    }
                    _ => "explorer.security.plan_sqlserver_add_role",
                }
            } else {
                match key {
                    "explorer.security.plan_sqlserver_remove_server_role" => {
                        "explorer.security.plan_sqlserver_remove_server_role"
                    }
                    _ => "explorer.security.plan_sqlserver_remove_role",
                }
            };
            return statement_description(key, &[("name", name), ("role", role)]);
        }
    }
    if let Some(rest) = text.strip_prefix("create user ") {
        if let Some((before, login)) = rest.split_once(" for login ")
            && let Some((name, database)) = before.split_once(" in ")
        {
            return statement_description(
                "explorer.security.plan_sqlserver_create_user_for_login",
                &[("name", name), ("database", database), ("login", login)],
            );
        }
        if let Some((name, database)) = rest.split_once(" in ") {
            if let Some(database) = database.strip_suffix(" without login") {
                return statement_description(
                    "explorer.security.plan_sqlserver_create_user_without_login",
                    &[("name", name), ("database", database)],
                );
            }
            return statement_description(
                "explorer.security.plan_sqlserver_create_user",
                &[("name", name), ("database", database)],
            );
        }
    }
    if let Some(rest) = text.strip_prefix("create contained user ")
        && let Some((name, database)) = rest.split_once(" in ")
    {
        return statement_description(
            "explorer.security.plan_sqlserver_create_contained_user",
            &[("name", name), ("database", database)],
        );
    }
    if let Some(rest) = text.strip_prefix("set default schema of ")
        && let Some((name, schema)) = rest.split_once(" to ")
    {
        return statement_description(
            "explorer.security.plan_sqlserver_set_default_schema",
            &[("name", name), ("schema", schema)],
        );
    }
    if let Some(rest) = text.strip_prefix("drop user ")
        && let Some((user, database)) = rest.split_once(" from ")
    {
        return statement_description(
            "explorer.security.plan_sqlserver_drop_user",
            &[("user", user), ("database", database)],
        );
    }
    if let Some((user, host)) = text
        .strip_prefix("drop ")
        .and_then(|rest| rest.split_once('@'))
    {
        return statement_description(
            "explorer.security.plan_mysql_drop_account",
            &[("user", user), ("host", host)],
        );
    }
    if let Some(role) = text.strip_prefix("drop ") {
        return statement_description(
            "explorer.security.plan_sqlserver_drop_role",
            &[("name", role)],
        );
    }
    summary
}

pub fn cell_text(value: &CellValue) -> String {
    match value {
        CellValue::Null => String::new(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => f.to_string(),
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(_) => String::new(),
    }
}

pub fn cell_opt_text(value: &CellValue) -> Option<String> {
    match value {
        CellValue::Null => None,
        other => Some(cell_text(other)),
    }
}

pub fn cell_bool(value: &CellValue) -> bool {
    match value {
        CellValue::Bool(b) => *b,
        CellValue::I64(n) => *n != 0,
        CellValue::String(s) => {
            s == "true" || s == "1" || s.eq_ignore_ascii_case("y") || s.eq_ignore_ascii_case("yes")
        }
        _ => false,
    }
}

pub fn name_taken(existing_names: &std::collections::BTreeSet<String>, name: &str) -> bool {
    existing_names
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(name))
}

pub fn one_line(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Characters a generated password draws from. Ambiguous glyphs (0/O, 1/l/I)
/// are left out so a password read off the screen types back correctly, and
/// the symbols avoid quotes, backslashes and `;`, which trip up connection
/// strings and shells even though the generated SQL escapes them.
const PASSWORD_LOWER: &[u8] = b"abcdefghijkmnopqrstuvwxyz";
const PASSWORD_UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
const PASSWORD_DIGITS: &[u8] = b"23456789";
const PASSWORD_SYMBOLS: &[u8] = b"!#$%&*+-=?@^_~";
pub const GENERATED_PASSWORD_LEN: usize = 20;

/// A random password that satisfies SQL Server's complexity policy (and the
/// usual PostgreSQL/MySQL/Oracle validators): every character class appears.
pub fn generate_password() -> String {
    let classes = [
        PASSWORD_LOWER,
        PASSWORD_UPPER,
        PASSWORD_DIGITS,
        PASSWORD_SYMBOLS,
    ];
    let all: Vec<u8> = classes.iter().flat_map(|c| c.iter().copied()).collect();
    let mut chars: Vec<u8> = classes.iter().map(|c| c[random_below(c.len())]).collect();
    while chars.len() < GENERATED_PASSWORD_LEN {
        chars.push(all[random_below(all.len())]);
    }
    // Shuffle so the guaranteed classes are not always the first four.
    for i in (1..chars.len()).rev() {
        chars.swap(i, random_below(i + 1));
    }
    String::from_utf8(chars).expect("password alphabet is ASCII")
}

/// Uniform in `0..n` (n <= 256). Rejection sampling keeps every value equally
/// likely; a plain `byte % n` would favour the low values.
fn random_below(n: usize) -> usize {
    use rand::Rng;
    let limit = 256 - (256 % n);
    let mut rng = rand::rng();
    loop {
        let mut byte = [0u8; 1];
        rng.fill_bytes(&mut byte);
        if (byte[0] as usize) < limit {
            return byte[0] as usize % n;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PasswordStrength {
    Weak,
    Fair,
    Strong,
}

pub(super) fn statement_description(key: &'static str, arguments: &[(&str, &str)]) -> String {
    crate::localization::format(key, arguments)
}

/// A rough guide, not a policy check: the server enforces its own rules.
/// Length and character variety are what count.
pub fn password_strength(password: &str) -> PasswordStrength {
    let len = password.chars().count();
    let classes = [
        password.chars().any(|c| c.is_lowercase()),
        password.chars().any(|c| c.is_uppercase()),
        password.chars().any(|c| c.is_ascii_digit()),
        password.chars().any(|c| !c.is_alphanumeric()),
    ]
    .iter()
    .filter(|present| **present)
    .count();
    if len >= 14 && classes >= 3 || len >= 20 {
        PasswordStrength::Strong
    } else if len >= 8 && classes >= 3 {
        PasswordStrength::Fair
    } else {
        PasswordStrength::Weak
    }
}

/// Stand-in shown instead of a password in the on-screen SQL.
pub const PASSWORD_MASK: &str = "********";

/// The display copy of a password: masked when set, empty stays empty (so the
/// plan keeps the same shape as the real one).
pub fn mask_password(password: &str) -> String {
    if password.is_empty() {
        String::new()
    } else {
        PASSWORD_MASK.to_string()
    }
}

#[cfg(test)]
mod password_tests {
    use super::*;

    #[test]
    fn generated_passwords_cover_every_class_and_differ() {
        for _ in 0..50 {
            let pw = generate_password();
            assert_eq!(pw.len(), GENERATED_PASSWORD_LEN);
            assert!(pw.bytes().any(|b| PASSWORD_LOWER.contains(&b)));
            assert!(pw.bytes().any(|b| PASSWORD_UPPER.contains(&b)));
            assert!(pw.bytes().any(|b| PASSWORD_DIGITS.contains(&b)));
            assert!(pw.bytes().any(|b| PASSWORD_SYMBOLS.contains(&b)));
            assert!(!pw.contains(['\'', '"', '\\', ';']));
            assert_eq!(password_strength(&pw), PasswordStrength::Strong);
        }
        assert_ne!(generate_password(), generate_password());
    }

    #[test]
    fn strength_grades_length_and_variety() {
        assert_eq!(password_strength("abc"), PasswordStrength::Weak);
        assert_eq!(password_strength("password"), PasswordStrength::Weak);
        assert_eq!(password_strength("Passw0rd"), PasswordStrength::Fair);
        assert_eq!(
            password_strength("Correct-Horse-9"),
            PasswordStrength::Strong
        );
        assert_eq!(
            password_strength("correcthorsebatterystaple"),
            PasswordStrength::Strong
        );
    }

    #[test]
    fn masking_keeps_empty_empty() {
        assert_eq!(mask_password(""), "");
        assert_eq!(mask_password("secret"), PASSWORD_MASK);
    }
}
