use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObjectId(Uuid);

impl ObjectId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ObjectId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectKind {
    Server,
    Database,
    Schema,
    Table,
    View,
    StoredProcedure,
    ScalarFunction,
    TableFunction,
    AggregateFunction,
    Trigger,
    Index,
    Statistic,
    Constraint,
    Column,
    Key,
    User,
    Role,
    Login,
    Filegroup,
    PartitionScheme,
    PartitionFunction,
    Synonym,
    Type,
    Rule,
    Default,
    Sequence,
    Extension,
    Event,
    MaterializedView,
    Assembly,
    Job,
    Schedule,
    Operator,
    Alert,
    Endpoint,
    LinkedServer,
    Credential,
    Certificate,
    SymmetricKey,
    AsymmetricKey,
    Service,
    Queue,
    Contract,
    MessageType,
    Route,
    BrokerPriority,
    ErrorLog,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectNode {
    pub id: ObjectId,
    pub kind: ObjectKind,
    pub name: String,
    pub schema: Option<String>,
    pub parent_id: Option<ObjectId>,
    pub is_expandable: bool,
    pub metadata: ObjectMetadata,
}

impl ObjectNode {
    pub fn new(kind: ObjectKind, name: String) -> Self {
        Self {
            id: ObjectId::new(),
            kind,
            name,
            schema: None,
            parent_id: None,
            is_expandable: false,
            metadata: ObjectMetadata::default(),
        }
    }

    pub fn with_schema(mut self, schema: String) -> Self {
        self.schema = Some(schema);
        self
    }

    pub fn expandable(mut self) -> Self {
        self.is_expandable = true;
        self
    }

    pub fn with_parent(mut self, parent: ObjectId) -> Self {
        self.parent_id = Some(parent);
        self
    }

    pub fn qualified_name(&self) -> String {
        match &self.schema {
            Some(s) => format!("{s}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ObjectMetadata {
    pub create_date: Option<chrono::DateTime<chrono::Utc>>,
    pub modify_date: Option<chrono::DateTime<chrono::Utc>>,
    pub is_system: bool,
    pub state: ObjectState,
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ObjectState {
    #[default]
    Online,
    Offline,
    Restoring,
    Recovering,
    Suspect,
    Emergency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScriptAction {
    Create,
    Alter,
    Drop,
    DropAndCreate,
    Select,
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptOptions {
    pub include_if_not_exists: bool,
    pub include_permissions: bool,
    pub include_indexes: bool,
    pub include_constraints: bool,
    pub include_triggers: bool,
    pub include_statistics: bool,
    pub include_extended_properties: bool,
    pub script_data: bool,
    pub script_defaults: bool,
    pub script_collation: bool,
    pub target_server_version: Option<String>,
}

impl Default for ScriptOptions {
    fn default() -> Self {
        Self {
            include_if_not_exists: false,
            include_permissions: false,
            include_indexes: true,
            include_constraints: true,
            include_triggers: false,
            include_statistics: false,
            include_extended_properties: false,
            script_data: false,
            script_defaults: true,
            script_collation: false,
            target_server_version: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseInfo {
    pub name: String,
    pub id: i32,
    pub state: ObjectState,
    pub create_date: chrono::DateTime<chrono::Utc>,
    pub collation: Option<String>,
    pub size_mb: Option<f64>,
    pub is_system: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub ordinal: i32,
    pub type_name: String,
    pub max_length: i32,
    pub precision: u8,
    pub scale: u8,
    pub is_nullable: bool,
    pub is_identity: bool,
    pub is_computed: bool,
    pub default_value: Option<String>,
    pub collation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexInfo {
    pub name: String,
    pub type_desc: String,
    pub is_unique: bool,
    pub is_primary_key: bool,
    pub columns: Vec<String>,
    pub included_columns: Vec<String>,
    pub filter_definition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstraintInfo {
    pub name: String,
    pub type_desc: String,
    pub columns: Vec<String>,
    pub definition: Option<String>,
}
