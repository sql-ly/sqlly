use sqlly_core::event::{CellValue, ResultColumn};

#[derive(Debug, Clone)]
pub struct TdsColumnMeta {
    pub name: String,
    pub type_name: String,
    pub type_id: u8,
    pub max_length: i32,
    pub precision: u8,
    pub scale: u8,
    pub nullable: bool,
    pub collation: Option<String>,
}

impl From<&TdsColumnMeta> for ResultColumn {
    fn from(meta: &TdsColumnMeta) -> Self {
        ResultColumn {
            name: meta.name.clone(),
            data_type: meta.type_name.clone(),
            nullable: meta.nullable,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TdsRow {
    pub values: Vec<CellValue>,
}

#[derive(Debug, Clone)]
pub struct TdsResultSet {
    pub columns: Vec<TdsColumnMeta>,
    pub rows: Vec<TdsRow>,
    pub row_count: u64,
}
