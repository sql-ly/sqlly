#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignerColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub identity: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDesignerModel {
    pub schema: String,
    pub table: String,
    pub columns: Vec<DesignerColumn>,
    pub primary_key: Vec<String>,
}

impl TableDesignerModel {
    pub fn create_table_sql(&self) -> String {
        let mut definitions = self
            .columns
            .iter()
            .map(|column| {
                format!(
                    "    {} {}{} {}",
                    quote_name(&column.name),
                    column.data_type,
                    if column.identity {
                        " IDENTITY(1,1)"
                    } else {
                        ""
                    },
                    if column.nullable { "NULL" } else { "NOT NULL" }
                )
            })
            .collect::<Vec<_>>();
        if !self.primary_key.is_empty() {
            definitions.push(format!(
                "    CONSTRAINT {} PRIMARY KEY ({})",
                quote_name(&format!("PK_{}", self.table)),
                self.primary_key
                    .iter()
                    .map(|column| quote_name(column))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        format!(
            "CREATE TABLE {}.{} (\n{}\n);",
            quote_name(&self.schema),
            quote_name(&self.table),
            definitions.join(",\n")
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDesignerModel {
    pub schema: String,
    pub table: String,
    pub name: String,
    pub columns: Vec<String>,
    pub include_columns: Vec<String>,
    pub unique: bool,
    pub filter: Option<String>,
    pub fill_factor: Option<u8>,
    pub clustered: bool,
}

impl IndexDesignerModel {
    pub fn create_index_sql(&self) -> String {
        let unique = if self.unique { "UNIQUE " } else { "" };
        let clustered = if self.clustered { "CLUSTERED " } else { "" };
        let columns = self
            .columns
            .iter()
            .map(|column| quote_name(column))
            .collect::<Vec<_>>()
            .join(", ");
        let include = if self.include_columns.is_empty() {
            String::new()
        } else {
            format!(
                " INCLUDE ({})",
                self.include_columns
                    .iter()
                    .map(|column| quote_name(column))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let mut sql = format!(
            "CREATE {unique}{clustered}INDEX {} ON {}.{} ({columns}){include}",
            quote_name(&self.name),
            quote_name(&self.schema),
            quote_name(&self.table),
        );
        if let Some(filter) = &self.filter {
            sql.push_str(&format!(" WHERE {filter}"));
        }
        if let Some(ff) = self.fill_factor {
            sql.push_str(&format!(" WITH (FILLFACTOR = {ff})"));
        }
        sql.push(';');
        sql
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationshipDesignerModel {
    pub name: String,
    pub child_schema: String,
    pub child_table: String,
    pub child_columns: Vec<String>,
    pub parent_schema: String,
    pub parent_table: String,
    pub parent_columns: Vec<String>,
}

impl RelationshipDesignerModel {
    pub fn add_foreign_key_sql(&self) -> String {
        format!(
            "ALTER TABLE {}.{} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}.{} ({});",
            quote_name(&self.child_schema),
            quote_name(&self.child_table),
            quote_name(&self.name),
            self.child_columns
                .iter()
                .map(|column| quote_name(column))
                .collect::<Vec<_>>()
                .join(", "),
            quote_name(&self.parent_schema),
            quote_name(&self.parent_table),
            self.parent_columns
                .iter()
                .map(|column| quote_name(column))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

pub fn create_or_alter_view_sql(schema: &str, view: &str, select_sql: &str) -> String {
    format!(
        "CREATE OR ALTER VIEW {}.{} AS\n{}",
        quote_name(schema),
        quote_name(view),
        select_sql.trim_end_matches(';')
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramNode {
    pub schema: String,
    pub table: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramEdge {
    pub foreign_key: String,
    pub from_table: String,
    pub to_table: String,
}

pub fn database_diagram_query() -> &'static str {
    "SELECT fk.name AS foreign_key, OBJECT_SCHEMA_NAME(fk.parent_object_id) AS child_schema, OBJECT_NAME(fk.parent_object_id) AS child_table, OBJECT_SCHEMA_NAME(fk.referenced_object_id) AS parent_schema, OBJECT_NAME(fk.referenced_object_id) AS parent_table FROM sys.foreign_keys fk ORDER BY child_schema, child_table, fk.name"
}

pub fn maintenance_plan_hint(name: &str) -> String {
    format!(
        "-- Maintenance plan placeholder for {}\n-- Prefer SQL Agent jobs with explicit backup, integrity check, and index maintenance steps.",
        quote_literal(name)
    )
}

pub fn policy_based_management_queries() -> [&'static str; 2] {
    [
        "SELECT name, is_enabled, date_created FROM msdb.dbo.syspolicy_policies ORDER BY name",
        "SELECT policy_id, target_query_expression, condition_id FROM msdb.dbo.syspolicy_target_sets ORDER BY policy_id",
    ]
}

pub fn replication_publications_query() -> &'static str {
    "SELECT name, publication, publisher_db, publication_type FROM syspublications ORDER BY name"
}

pub fn linked_servers_query() -> &'static str {
    "SELECT name, product, provider, data_source, is_linked FROM sys.servers WHERE is_linked = 1 ORDER BY name"
}

pub fn ssis_catalog_query() -> &'static str {
    "SELECT name, created_time FROM SSISDB.catalog.folders ORDER BY name"
}

pub fn ssas_connection_hint(server: &str) -> String {
    format!(
        "Use XMLA/TMSL tooling for Analysis Services administration against server '{}'.",
        quote_literal(server)
    )
}

pub fn ssrs_catalog_query() -> &'static str {
    "SELECT Path, Name, Type, ModifiedDate FROM ReportServer.dbo.Catalog ORDER BY Path"
}

pub fn central_management_servers_query() -> &'static str {
    "SELECT server_name, description FROM msdb.dbo.sysmanagement_shared_registered_servers_internal ORDER BY server_name"
}

pub fn custom_report_manifest_supported(extension: &str) -> bool {
    matches!(extension.to_ascii_lowercase().as_str(), "rdl" | "rdlc")
}

fn quote_name(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn quote_literal(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_index_relationship_and_view_designers_script_sql() {
        let table = TableDesignerModel {
            schema: "dbo".to_string(),
            table: "Orders".to_string(),
            columns: vec![DesignerColumn {
                name: "OrderId".to_string(),
                data_type: "int".to_string(),
                nullable: false,
                identity: true,
            }],
            primary_key: vec!["OrderId".to_string()],
        };
        assert!(table.create_table_sql().contains("IDENTITY(1,1) NOT NULL"));
        assert!(table.create_table_sql().contains("CONSTRAINT [PK_Orders]"));

        let index = IndexDesignerModel {
            schema: "dbo".to_string(),
            table: "Orders".to_string(),
            name: "IX_Orders_Customer".to_string(),
            columns: vec!["CustomerId".to_string()],
            include_columns: vec!["Total".to_string()],
            unique: false,
            filter: None,
            fill_factor: None,
            clustered: false,
        };
        assert!(index.create_index_sql().contains("INCLUDE ([Total])"));

        let filtered = IndexDesignerModel {
            filter: Some("IsActive = 1".to_string()),
            fill_factor: Some(90),
            ..index.clone()
        };
        assert!(filtered.create_index_sql().contains("WHERE IsActive = 1"));
        assert!(filtered.create_index_sql().contains("FILLFACTOR = 90"));

        let clustered = IndexDesignerModel {
            clustered: true,
            include_columns: vec![],
            ..index
        };
        assert!(clustered.create_index_sql().contains("CLUSTERED INDEX"));

        let relationship = RelationshipDesignerModel {
            name: "FK_Orders_Customers".to_string(),
            child_schema: "dbo".to_string(),
            child_table: "Orders".to_string(),
            child_columns: vec!["CustomerId".to_string()],
            parent_schema: "dbo".to_string(),
            parent_table: "Customers".to_string(),
            parent_columns: vec!["CustomerId".to_string()],
        };
        assert!(
            relationship
                .add_foreign_key_sql()
                .contains("FOREIGN KEY ([CustomerId])")
        );
        assert!(create_or_alter_view_sql("dbo", "v", "SELECT 1;").ends_with("SELECT 1"));
    }

    #[test]
    fn long_tail_admin_queries_cover_phase_12_scope() {
        assert!(database_diagram_query().contains("sys.foreign_keys"));
        assert!(maintenance_plan_hint("Nightly's").contains("Nightly''s"));
        assert!(policy_based_management_queries()[0].contains("syspolicy_policies"));
        assert!(replication_publications_query().contains("syspublications"));
        assert!(linked_servers_query().contains("sys.servers"));
        assert!(ssis_catalog_query().contains("SSISDB.catalog.folders"));
        assert!(ssas_connection_hint("as'server").contains("as''server"));
        assert!(ssrs_catalog_query().contains("ReportServer.dbo.Catalog"));
        assert!(central_management_servers_query().contains("registered_servers"));
        assert!(custom_report_manifest_supported("RDL"));
        assert!(!custom_report_manifest_supported("txt"));
    }
}
