use crate::scripting::{ScriptingError, ScriptingResult, identifier::IdentifierQuoter};
use sqlly_core::{ObjectKind, ObjectNode, ScriptAction, ScriptOptions};

pub struct ScriptGenerator {
    quoter: IdentifierQuoter,
}

impl ScriptGenerator {
    pub fn new() -> Self {
        Self {
            quoter: IdentifierQuoter::new(),
        }
    }

    pub fn generate(
        &self,
        node: &ObjectNode,
        action: &ScriptAction,
        options: &ScriptOptions,
    ) -> ScriptingResult<String> {
        match action {
            ScriptAction::Create => self.generate_create(node, options),
            ScriptAction::Alter => self.generate_alter(node, options),
            ScriptAction::Drop => self.generate_drop(node, options),
            ScriptAction::DropAndCreate => {
                let drop = self.generate_drop(node, options)?;
                let create = self.generate_create(node, options)?;
                Ok(format!("{drop}\nGO\n\n{create}"))
            }
            ScriptAction::Select => self.generate_select(node),
            ScriptAction::Insert => self.generate_insert(node),
            ScriptAction::Update => self.generate_update(node),
            ScriptAction::Delete => self.generate_delete(node),
        }
    }

    fn generate_create(
        &self,
        node: &ObjectNode,
        options: &ScriptOptions,
    ) -> ScriptingResult<String> {
        let name = self
            .quoter
            .quote_qualified(node.schema.as_deref(), &node.name);
        let mut script = match node.kind {
            ObjectKind::Table => {
                format!("CREATE TABLE {name}\n(\n  id INT NOT NULL IDENTITY(1,1) PRIMARY KEY\n)")
            }
            ObjectKind::View => format!("CREATE VIEW {name}\nAS\nSELECT 1 AS [value]"),
            ObjectKind::StoredProcedure => {
                format!("CREATE PROCEDURE {name}\nAS\nBEGIN\n  RETURN 0\nEND")
            }
            ObjectKind::ScalarFunction
            | ObjectKind::TableFunction
            | ObjectKind::AggregateFunction => {
                format!("CREATE FUNCTION {name}()\nRETURNS INT\nAS\nBEGIN\n  RETURN 0\nEND")
            }
            ObjectKind::Trigger => format!(
                "CREATE TRIGGER {name}\nON <schema_name, sysname, dbo>.<table_name, sysname, table_name>\nAFTER INSERT\nAS\nBEGIN\n  SET NOCOUNT ON;\nEND"
            ),
            ObjectKind::Schema => format!("CREATE SCHEMA {name}"),
            ObjectKind::Role => format!("CREATE ROLE {name}"),
            ObjectKind::User => format!("CREATE USER {name} WITHOUT LOGIN"),
            ObjectKind::Index => format!(
                "CREATE INDEX {name}\nON {}\n(\n  <column_name, sysname, column_name> ASC\n)",
                self.parent_object_name(node)
            ),
            ObjectKind::Constraint => format!(
                "ALTER TABLE {}\nADD CONSTRAINT {name} CHECK (<search_condition>)",
                self.parent_object_name(node)
            ),
            ObjectKind::Synonym => format!(
                "CREATE SYNONYM {name}\nFOR <server_name, sysname, server_name>.<database_name, sysname, database_name>.<schema_name, sysname, schema_name>.<object_name, sysname, object_name>"
            ),
            _ => return Err(ScriptingError::UnsupportedType(node.kind.clone())),
        };
        if options.include_if_not_exists {
            script = format!(
                "IF OBJECT_ID(N'{}') IS NULL\nBEGIN\n{}\nEND",
                node.qualified_name().replace('\'', "''"),
                indent_block(&script)
            );
        }
        if options.include_permissions {
            script.push_str("\n\n-- Permissions: script GRANT/DENY statements after reviewing source object permissions.");
        }
        Ok(script)
    }

    fn generate_alter(
        &self,
        node: &ObjectNode,
        _options: &ScriptOptions,
    ) -> ScriptingResult<String> {
        let name = self
            .quoter
            .quote_qualified(node.schema.as_deref(), &node.name);
        let script = match node.kind {
            ObjectKind::View => format!("ALTER VIEW {name}\nAS\nSELECT 1"),
            ObjectKind::StoredProcedure => {
                format!("ALTER PROCEDURE {name}\nAS\nBEGIN\n  RETURN 0\nEND")
            }
            ObjectKind::ScalarFunction
            | ObjectKind::TableFunction
            | ObjectKind::AggregateFunction => {
                format!("ALTER FUNCTION {name}()\nRETURNS INT\nAS\nBEGIN\n  RETURN 0\nEND")
            }
            ObjectKind::Trigger => format!(
                "ALTER TRIGGER {name}\nON <schema_name, sysname, dbo>.<table_name, sysname, table_name>\nAFTER INSERT\nAS\nBEGIN\n  SET NOCOUNT ON;\nEND"
            ),
            _ => return Err(ScriptingError::UnsupportedType(node.kind.clone())),
        };
        Ok(script)
    }

    fn generate_drop(
        &self,
        node: &ObjectNode,
        _options: &ScriptOptions,
    ) -> ScriptingResult<String> {
        let name = self
            .quoter
            .quote_qualified(node.schema.as_deref(), &node.name);
        let keyword = match node.kind {
            ObjectKind::Table => "TABLE",
            ObjectKind::View => "VIEW",
            ObjectKind::StoredProcedure => "PROCEDURE",
            ObjectKind::ScalarFunction
            | ObjectKind::TableFunction
            | ObjectKind::AggregateFunction => "FUNCTION",
            ObjectKind::Trigger => "TRIGGER",
            ObjectKind::Schema => "SCHEMA",
            ObjectKind::Role => "ROLE",
            ObjectKind::User => "USER",
            ObjectKind::Synonym => "SYNONYM",
            ObjectKind::Constraint => "CONSTRAINT",
            ObjectKind::Index => {
                return Ok(format!(
                    "DROP INDEX {name}\nON {}",
                    self.parent_object_name(node)
                ));
            }
            _ => return Err(ScriptingError::UnsupportedType(node.kind.clone())),
        };
        let mut script = format!("DROP {keyword} {name}");
        if matches!(node.kind, ObjectKind::Table | ObjectKind::View) {
            script = format!(
                "-- Dependency warning: review referencing objects before running this script.\n{script}"
            );
        }
        Ok(script)
    }

    fn generate_select(&self, node: &ObjectNode) -> ScriptingResult<String> {
        let name = self
            .quoter
            .quote_qualified(node.schema.as_deref(), &node.name);
        Ok(format!("SELECT TOP 1000 *\nFROM {name}"))
    }

    fn generate_insert(&self, node: &ObjectNode) -> ScriptingResult<String> {
        let name = self
            .quoter
            .quote_qualified(node.schema.as_deref(), &node.name);
        Ok(format!(
            "INSERT INTO {name}\n(-- columns\n)\nVALUES\n(-- values\n)"
        ))
    }

    fn generate_update(&self, node: &ObjectNode) -> ScriptingResult<String> {
        let name = self
            .quoter
            .quote_qualified(node.schema.as_deref(), &node.name);
        Ok(format!(
            "UPDATE {name}\nSET -- column = value\nWHERE -- condition"
        ))
    }

    fn generate_delete(&self, node: &ObjectNode) -> ScriptingResult<String> {
        let name = self
            .quoter
            .quote_qualified(node.schema.as_deref(), &node.name);
        Ok(format!("DELETE FROM {name}\nWHERE -- condition"))
    }

    fn parent_object_name(&self, node: &ObjectNode) -> String {
        let parent = node
            .metadata
            .extra
            .get("parent")
            .and_then(|value| value.as_str())
            .or_else(|| {
                node.metadata
                    .extra
                    .get("table")
                    .and_then(|value| value.as_str())
            })
            .unwrap_or("<schema_name, sysname, dbo>.<table_name, sysname, table_name>");
        parent
            .split('.')
            .map(|part| self.quoter.quote(part.trim_matches(['[', ']'])))
            .collect::<Vec<_>>()
            .join(".")
    }
}

fn indent_block(script: &str) -> String {
    script
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Default for ScriptGenerator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::ObjectMetadata;

    fn node(kind: ObjectKind, schema: &str, name: &str) -> ObjectNode {
        ObjectNode::new(kind, name.to_string()).with_schema(schema.to_string())
    }

    #[test]
    fn scripts_drop_and_create_with_dependency_warning() {
        let generator = ScriptGenerator::new();
        let sql = generator
            .generate(
                &node(ObjectKind::View, "dbo", "active customers"),
                &ScriptAction::DropAndCreate,
                &ScriptOptions::default(),
            )
            .unwrap();

        assert!(sql.contains("Dependency warning"));
        assert!(sql.contains("DROP VIEW [dbo].[active customers]"));
        assert!(sql.contains("CREATE VIEW [dbo].[active customers]"));
    }

    #[test]
    fn scripts_index_with_parent_table() {
        let generator = ScriptGenerator::new();
        let mut index = node(ObjectKind::Index, "dbo", "IX_orders_customer");
        index.metadata = ObjectMetadata {
            extra: serde_json::json!({ "table": "sales.orders" }),
            ..ObjectMetadata::default()
        };

        let create = generator
            .generate(&index, &ScriptAction::Create, &ScriptOptions::default())
            .unwrap();
        let drop = generator
            .generate(&index, &ScriptAction::Drop, &ScriptOptions::default())
            .unwrap();

        assert!(create.contains("CREATE INDEX [dbo].[IX_orders_customer]"));
        assert!(create.contains("ON sales.orders"));
        assert!(drop.contains("DROP INDEX [dbo].[IX_orders_customer]"));
    }

    #[test]
    fn create_script_honors_if_not_exists_and_permissions_options() {
        let generator = ScriptGenerator::new();
        let options = ScriptOptions {
            include_if_not_exists: true,
            include_permissions: true,
            ..ScriptOptions::default()
        };
        let sql = generator
            .generate(
                &node(ObjectKind::Role, "dbo", "report_reader"),
                &ScriptAction::Create,
                &options,
            )
            .unwrap();

        assert!(sql.starts_with("IF OBJECT_ID(N'dbo.report_reader') IS NULL"));
        assert!(sql.contains("CREATE ROLE [dbo].[report_reader]"));
        assert!(sql.contains("Permissions"));
    }

    #[test]
    fn scripts_synonym_user_role_and_constraint() {
        let generator = ScriptGenerator::new();
        for (kind, expected) in [
            (ObjectKind::Synonym, "CREATE SYNONYM [dbo].[remote_orders]"),
            (
                ObjectKind::User,
                "CREATE USER [dbo].[remote_orders] WITHOUT LOGIN",
            ),
            (ObjectKind::Role, "CREATE ROLE [dbo].[remote_orders]"),
            (
                ObjectKind::Constraint,
                "ADD CONSTRAINT [dbo].[remote_orders] CHECK",
            ),
        ] {
            let sql = generator
                .generate(
                    &node(kind, "dbo", "remote_orders"),
                    &ScriptAction::Create,
                    &ScriptOptions::default(),
                )
                .unwrap();
            assert!(sql.contains(expected), "{sql}");
        }
    }
}
