#![forbid(unsafe_code)]

pub mod diff;
pub mod script;
pub mod snapshot;

pub use diff::{Change, ChangeSet, TableRef, diff, order_key, touches_columns};
pub use script::comments::{
    self, CommentChange, CommentObjectKind, render_comment_change, supports_comments,
    unsupported_reason, unsupported_reason_for, unsupported_reason_for_message,
    unsupported_reason_message,
};
pub use script::create::{
    CreateScript, render_create_tables, renders_from_snapshot, script_tables_as_create,
};
pub use script::{AlterError, AlterScript, MysqlAlgorithm, MysqlLock, RenderOptions, render};
pub use snapshot::{
    CheckConstraintDef, ColumnDef, ColumnExtras, DependentView, DraftColumn, ForeignKeyDef,
    GeneratedColumn, IndexDef, TableDraft, TableSnapshot, TableStorage,
};
