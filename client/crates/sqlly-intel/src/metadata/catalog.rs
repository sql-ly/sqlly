pub struct CatalogQuery;

impl CatalogQuery {
    pub fn list_databases() -> &'static str {
        "SELECT name, database_id, state_desc, create_date, collation_name FROM sys.databases ORDER BY name"
    }

    pub fn list_tables(schema: &str) -> String {
        format!(
            "SELECT t.name, s.name AS schema_name, t.create_date, t.modify_date \
             FROM sys.tables t \
             INNER JOIN sys.schemas s ON t.schema_id = s.schema_id \
             WHERE s.name = '{schema}' \
             ORDER BY t.name"
        )
    }

    pub fn list_views(schema: &str) -> String {
        format!(
            "SELECT v.name, s.name AS schema_name, v.create_date, v.modify_date \
             FROM sys.views v \
             INNER JOIN sys.schemas s ON v.schema_id = s.schema_id \
             WHERE s.name = '{schema}' \
             ORDER BY v.name"
        )
    }

    pub fn list_stored_procedures(schema: &str) -> String {
        format!(
            "SELECT p.name, s.name AS schema_name, p.create_date, p.modify_date \
             FROM sys.procedures p \
             INNER JOIN sys.schemas s ON p.schema_id = s.schema_id \
             WHERE s.name = '{schema}' AND p.type = 'P' \
             ORDER BY p.name"
        )
    }

    pub fn list_columns(table_id: i32) -> String {
        format!(
            "SELECT c.name, c.column_id, tp.name AS type_name, c.max_length, \
             c.precision, c.scale, c.is_nullable, c.is_identity, c.is_computed, \
             dc.definition AS default_value, c.collation_name \
             FROM sys.columns c \
             INNER JOIN sys.types tp ON c.user_type_id = tp.user_type_id \
             LEFT JOIN sys.default_constraints dc ON c.default_object_id = dc.object_id \
             WHERE c.object_id = {table_id} \
             ORDER BY c.column_id"
        )
    }

    pub fn server_info() -> &'static str {
        "SELECT SERVERPROPERTY('ProductVersion') AS version, \
         SERVERPROPERTY('Edition') AS edition, \
         SERVERPROPERTY('EngineEdition') AS engine_edition, \
         SERVERPROPERTY('ProductLevel') AS product_level"
    }
}
