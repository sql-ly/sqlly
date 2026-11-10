pub use sqlly_core::{ObjectId, ObjectKind, ObjectNode};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeId {
    Root,
    Databases,
    Database(ObjectId),
    Tables(ObjectId),
    Views(ObjectId),
    Programmability(ObjectId),
    Security(ObjectId),
    Storage(ObjectId),
    Table(ObjectId),
    View(ObjectId),
    Procedure(ObjectId),
    Function(ObjectId),
    Columns(ObjectId),
    Indexes(ObjectId),
    Triggers(ObjectId),
}
