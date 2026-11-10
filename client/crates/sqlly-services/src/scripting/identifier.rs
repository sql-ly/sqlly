#[derive(Debug, Clone)]
pub struct IdentifierQuoter;

impl IdentifierQuoter {
    pub fn new() -> Self {
        Self
    }

    pub fn quote(&self, identifier: &str) -> String {
        if self.needs_quoting(identifier) {
            self.quote_always(identifier)
        } else {
            identifier.to_string()
        }
    }

    pub fn quote_qualified(&self, schema: Option<&str>, name: &str) -> String {
        match schema {
            Some(s) => format!("{}.{}", self.quote_always(s), self.quote_always(name)),
            None => self.quote_always(name),
        }
    }

    pub fn quote_multipart(&self, parts: &[&str]) -> String {
        parts
            .iter()
            .map(|p| self.quote_always(p))
            .collect::<Vec<_>>()
            .join(".")
    }

    fn quote_always(&self, identifier: &str) -> String {
        format!("[{}]", identifier.replace(']', "]]"))
    }

    fn needs_quoting(&self, identifier: &str) -> bool {
        if identifier.is_empty() {
            return true;
        }
        let first = identifier.chars().next().unwrap();
        if first.is_ascii_uppercase() {
            return true;
        }
        if !first.is_ascii_alphabetic() && first != '_' && first != '#' {
            return true;
        }
        for ch in identifier.chars() {
            if !ch.is_ascii_alphanumeric() && ch != '_' {
                return true;
            }
        }
        let lower = identifier.to_lowercase();
        matches!(
            lower.as_str(),
            "select"
                | "insert"
                | "update"
                | "delete"
                | "create"
                | "alter"
                | "drop"
                | "from"
                | "where"
                | "join"
                | "inner"
                | "outer"
                | "left"
                | "right"
                | "full"
                | "on"
                | "and"
                | "or"
                | "not"
                | "null"
                | "is"
                | "in"
                | "between"
                | "like"
                | "as"
                | "order"
                | "by"
                | "group"
                | "having"
                | "union"
                | "all"
                | "distinct"
                | "set"
                | "values"
                | "into"
                | "table"
                | "view"
                | "index"
                | "procedure"
                | "function"
                | "trigger"
                | "schema"
                | "database"
                | "server"
                | "user"
                | "role"
                | "login"
                | "grant"
                | "revoke"
                | "deny"
                | "exec"
                | "execute"
                | "go"
                | "use"
                | "if"
                | "else"
                | "begin"
                | "end"
                | "return"
                | "declare"
                | "while"
                | "break"
                | "continue"
                | "waitfor"
                | "try"
                | "catch"
                | "throw"
                | "raiserror"
                | "print"
                | "cast"
                | "convert"
        )
    }
}

impl Default for IdentifierQuoter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_identifier() {
        let q = IdentifierQuoter::new();
        assert_eq!(q.quote("MyTable"), "[MyTable]");
    }

    #[test]
    fn test_qualified_name() {
        let q = IdentifierQuoter::new();
        assert_eq!(q.quote_qualified(Some("dbo"), "MyTable"), "[dbo].[MyTable]");
    }

    #[test]
    fn test_reserved_word() {
        let q = IdentifierQuoter::new();
        assert_eq!(q.quote("select"), "[select]");
    }

    #[test]
    fn test_non_reserved_word() {
        let q = IdentifierQuoter::new();
        assert_eq!(q.quote("customers"), "customers");
    }

    #[test]
    fn test_identifier_with_spaces() {
        let q = IdentifierQuoter::new();
        assert_eq!(q.quote("My Table"), "[My Table]");
    }

    #[test]
    fn test_quote_doubles_closing_bracket() {
        let q = IdentifierQuoter::new();
        assert_eq!(q.quote("my]col"), "[my]]col]");
    }

    #[test]
    fn test_multipart() {
        let q = IdentifierQuoter::new();
        assert_eq!(
            q.quote_multipart(&["server", "database", "dbo", "table"]),
            "[server].[database].[dbo].[table]"
        );
    }
}
