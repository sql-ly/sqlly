//! Procedure intellisense — port of `ProcedureIntelliSense.swift`.
//!
//! Pure, UI-free. Generates declaration snippets for stored procedures using
//! slot-type placeholder literals. Adapts the Swift `ProcedureIntelliSense`
//! (which built `PROCEDURE` completion snippets from parameter metadata) to
//! the flat Rust model. Shared by the gpui editor and the LSP server.

use super::slot_type::placeholder_literal;

/// A single procedure parameter descriptor used to build a snippet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcedureParameter {
    pub name: String,
    pub data_type: String,
    pub is_output: bool,
}

impl ProcedureParameter {
    pub fn new(name: impl Into<String>, data_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            data_type: data_type.into(),
            is_output: false,
        }
    }

    pub fn output(mut self) -> Self {
        self.is_output = true;
        self
    }
}

/// A stored procedure descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcedureInfo {
    pub schema: String,
    pub name: String,
    pub parameters: Vec<ProcedureParameter>,
}

impl ProcedureInfo {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            name: name.into(),
            parameters: Vec::new(),
        }
    }

    /// Two-part display name: `schema.name`.
    pub fn display_name(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    /// Generates an `EXEC` declaration snippet with parameter placeholders.
    /// e.g. `EXEC [dbo].[MyProc] @id = 0, @name = N''`
    /// Parameters are comma-separated; `OUTPUT` suffix is appended for output
    /// parameters.
    pub fn declaration_snippet(&self) -> String {
        let mut s = format!("EXEC {} ", self.qualified_name());
        if self.parameters.is_empty() {
            s.push_str("; -- no parameters");
            return s;
        }
        let parts: Vec<String> = self
            .parameters
            .iter()
            .map(|p| {
                let placeholder = placeholder_literal(&p.data_type);
                let mut line = format!("{} = {}", p.name, placeholder);
                if p.is_output {
                    line.push_str(" OUTPUT");
                }
                line
            })
            .collect();
        s.push_str(&parts.join(", "));
        s
    }

    /// Bracket-qualified `[schema].[name]`.
    pub fn qualified_name(&self) -> String {
        format!(
            "[{}].[{}]",
            self.schema.replace(']', "]]"),
            self.name.replace(']', "]]")
        )
    }

    /// Parameter list as displayed in a tooltip, one per line.
    pub fn parameter_list_text(&self) -> String {
        if self.parameters.is_empty() {
            return "(no parameters)".to_string();
        }
        self.parameters
            .iter()
            .map(|p| {
                let suffix = if p.is_output { " (OUTPUT)" } else { "" };
                format!("{} {}{}", p.data_type, p.name, suffix)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name() {
        let p = ProcedureInfo::new("dbo", "GetUser");
        assert_eq!(p.display_name(), "dbo.GetUser");
    }

    #[test]
    fn qualified_name_brackets() {
        let p = ProcedureInfo::new("dbo", "GetUser");
        assert_eq!(p.qualified_name(), "[dbo].[GetUser]");
    }

    #[test]
    fn snippet_no_params() {
        let p = ProcedureInfo::new("dbo", "GetVersion");
        let snippet = p.declaration_snippet();
        assert!(snippet.starts_with("EXEC [dbo].[GetVersion]"));
        assert!(snippet.contains("no parameters"));
    }

    #[test]
    fn snippet_with_params() {
        let mut p = ProcedureInfo::new("dbo", "GetUser");
        p.parameters.push(ProcedureParameter::new("@id", "int"));
        p.parameters
            .push(ProcedureParameter::new("@name", "nvarchar(50)"));
        let snippet = p.declaration_snippet();
        assert!(snippet.contains("@id = 0"));
        assert!(snippet.contains("@name = N''"));
        assert!(snippet.contains(", "));
    }

    #[test]
    fn snippet_with_output_param() {
        let mut p = ProcedureInfo::new("dbo", "GetCount");
        p.parameters.push(ProcedureParameter::new("@id", "int"));
        p.parameters
            .push(ProcedureParameter::new("@count", "int").output());
        let snippet = p.declaration_snippet();
        assert!(snippet.contains("@id = 0"));
        assert!(snippet.contains("@count = 0 OUTPUT"));
    }

    #[test]
    fn snippet_date_param_uses_empty_string() {
        let mut p = ProcedureInfo::new("dbo", "GetByDate");
        p.parameters
            .push(ProcedureParameter::new("@date", "datetime2"));
        let snippet = p.declaration_snippet();
        assert!(snippet.contains("@date = ''"));
    }

    #[test]
    fn parameter_list_text_empty() {
        let p = ProcedureInfo::new("dbo", "NoParams");
        assert_eq!(p.parameter_list_text(), "(no parameters)");
    }

    #[test]
    fn parameter_list_text_with_params() {
        let mut p = ProcedureInfo::new("dbo", "Multi");
        p.parameters.push(ProcedureParameter::new("@id", "int"));
        p.parameters
            .push(ProcedureParameter::new("@name", "nvarchar(100)").output());
        let text = p.parameter_list_text();
        assert!(text.contains("int @id"));
        assert!(text.contains("nvarchar(100) @name (OUTPUT)"));
        assert_eq!(text.matches('\n').count(), 1);
    }

    #[test]
    fn snippet_exec_prefix() {
        let p = ProcedureInfo::new("dbo", "MyProc");
        let snippet = p.declaration_snippet();
        assert!(snippet.starts_with("EXEC [dbo].[MyProc]"));
    }
}
